//! End-to-end smoke test: build a workspace twice; first is a cache miss,
//! second is a cache hit that restores outputs from CAS.

use std::process::Command;

fn giant_bin() -> std::path::PathBuf {
    // Cargo sets CARGO_BIN_EXE_<name> for the bin of the package under test.
    let path = env!("CARGO_BIN_EXE_giant");
    std::path::PathBuf::from(path)
}

fn fixture_path(name: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Recursively copy a directory tree. Preserves the executable bit so
/// fixture scripts stay runnable after copy.
fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(&src_path)?.permissions().mode();
                std::fs::set_permissions(&dst_path, std::fs::Permissions::from_mode(mode))?;
            }
        }
    }
    Ok(())
}

fn have_program(name: &str) -> bool {
    Command::new(name)
        .arg("version")
        .output()
        .or_else(|_| Command::new(name).arg("--version").output())
        .map(|o| o.status.success() || !o.stdout.is_empty() || !o.stderr.is_empty())
        .unwrap_or(false)
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
  - id: "demo:hello"
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
  - id: "a"
    inputs: []
    outputs: ["a.txt"]
    command: "echo a > a.txt"
  - id: "b"
    inputs: ["a.txt"]
    outputs: ["b.txt"]
    deps: ["a"]
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
  - id: "bad"
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
        stdout.contains("FAILED") || stderr.contains("failed"),
        "expected failure mention; stdout={stdout} stderr={stderr}"
    );
}

#[test]
fn early_cutoff_byte_identical_upstream_doesnt_invalidate_downstream() {
    // Property under test (TDD-0009 §Early-cutoff): if an upstream target
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
  - id: "a"
    inputs: ["a.in"]
    outputs: ["a.out"]
    command: "echo constant > a.out"   # deterministic output regardless of a.in
  - id: "b"
    inputs: ["a.out"]
    outputs: ["b.out"]
    deps: ["a"]
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
    assert!(s1.contains("built  a"), "expected a built; got: {s1}");
    assert!(s1.contains("built  b"), "expected b built; got: {s1}");

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
        s2.contains("built  a"),
        "expected a to rebuild (its input changed); got: {s2}"
    );
    assert!(
        s2.contains("cache  b"),
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
  - id: "a"
    inputs: []
    outputs: ["a.txt"]
    cache: false
    command: "sleep 0.3 && echo a > a.txt"
  - id: "b"
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
  - id: "a"
    inputs: []
    outputs: ["a.txt"]
    cache: false
    command: "sleep 0.3 && echo a > a.txt"
  - id: "b"
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
fn output_based_inference_links_static_targets() {
    // 'b' has input matching 'a's output. Engine infers b depends on a
    // without an explicit deps: declaration.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: inferlinks
cache:
  dir: ./cache
targets:
  - id: "a"
    inputs: []
    outputs: ["gen.txt"]
    command: "echo from-a > gen.txt"
  - id: "b"
    inputs: ["gen.txt"]
    outputs: ["out.txt"]
    command: "cat gen.txt > out.txt"
"#,
    )
    .unwrap();
    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("built  a"), "a should build; got: {s}");
    assert!(s.contains("built  b"), "b should build; got: {s}");
    // Verify b ran after a - b's output depends on a's having run first.
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap().trim(),
        "from-a"
    );
}

#[test]
fn discovery_include_bootstraps_and_merges() {
    // A discovery target emits JSON, the engine merges it, and a static
    // downstream target ends up correctly depending on the discovered
    // target via output-based inference.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir(ws.join("tools")).unwrap();
    std::fs::write(
        ws.join("tools/discover.sh"),
        r#"#!/usr/bin/env bash
set -euo pipefail
mkdir -p .giant/d
cat > .giant/d/svc.json <<'JSON'
{
  "targets": [
    {
      "id": "svc:hello",
      "inputs": [],
      "outputs": ["svc-hello.txt"],
      "command": "echo discovered_hello > svc-hello.txt"
    }
  ]
}
JSON
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = std::fs::metadata(ws.join("tools/discover.sh"))
            .unwrap()
            .permissions();
        let mut p = p;
        p.set_mode(0o755);
        std::fs::set_permissions(ws.join("tools/discover.sh"), p).unwrap();
    }

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: discovery
cache:
  dir: ./cache
include:
  - id: "discover:svc"
    inputs: ["tools/discover.sh"]
    outputs: [".giant/d/svc.json"]
    command: "./tools/discover.sh"
targets:
  - id: "downstream"
    inputs: ["svc-hello.txt"]
    outputs: ["combined.txt"]
    command: "echo combined: > combined.txt && cat svc-hello.txt >> combined.txt"
"#,
    )
    .unwrap();

    // Cold run: bootstrap builds discover:svc, merge picks up svc:hello,
    // inference wires downstream -> svc:hello via the input/output match,
    // main build runs svc:hello and downstream in dep order.
    let out1 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out1.status.success(), "cold build failed: {}", String::from_utf8_lossy(&out1.stderr));
    let s1 = String::from_utf8_lossy(&out1.stdout);
    assert!(s1.contains("built  discover:svc"), "discover should build; {s1}");
    assert!(s1.contains("built  svc:hello"), "svc:hello should build; {s1}");
    assert!(s1.contains("built  downstream"), "downstream should build; {s1}");
    assert_eq!(
        std::fs::read_to_string(ws.join("combined.txt"))
            .unwrap()
            .trim(),
        "combined:\ndiscovered_hello"
    );

    // Warm run: everything cache-hits. Proves the inferred dep doesn't
    // race with svc:hello's restore (downstream's cache key must remain
    // stable across runs).
    let out2 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out2.status.success());
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(s2.contains("cache  discover:svc"));
    assert!(s2.contains("cache  svc:hello"));
    assert!(
        s2.contains("cache  downstream"),
        "downstream must cache-hit on warm run (deterministic cache key); got: {s2}"
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
  - id: "docker:img"
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
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("external"), "expected external hit; got: {s}");
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
  - id: "docker:img"
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
    assert!(s.contains("built  docker:img"), "expected build to run; got: {s}");
    assert_eq!(
        std::fs::read_to_string(ws.join("receipt.txt")).unwrap().trim(),
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
  - id: "docker:img"
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
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("external"), "expected external hit; got: {s}");
    assert!(!ws.join("marker.txt").exists());
}

#[test]
fn structural_input_ignores_function_body_edits() {
    // Property under test (TDD-0002 §Per-input fingerprint): a target
    // with a structural input matching `^package ` and `^import ` lines
    // in *.go files. Editing a function body - none of the matching
    // lines change - must leave the target's cache key stable, so a
    // rebuild after the edit cache-hits.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("src/main.go"),
        "package main\nimport \"fmt\"\nfunc main() { fmt.Println(\"v1\") }\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: structural
cache:
  dir: ./cache
targets:
  - id: "discover:go"
    inputs:
      - "go.mod"
      - kind: structural
        files: "src/**/*.go"
        lines: ["package ", "import "]
    outputs: ["discovered.json"]
    command: 'echo "{\"targets\": []}" > discovered.json'
"#,
    )
    .unwrap();
    std::fs::write(ws.join("go.mod"), "module x\n").unwrap();

    // First build - cold.
    let out1 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out1.status.success(), "first build failed: {}", String::from_utf8_lossy(&out1.stderr));
    let s1 = String::from_utf8_lossy(&out1.stdout);
    assert!(s1.contains("built  discover:go"), "got: {s1}");

    // Edit the function body but not the package/import lines.
    std::fs::write(
        ws.join("src/main.go"),
        "package main\nimport \"fmt\"\nfunc main() { fmt.Println(\"v2-totally-different\") }\n",
    )
    .unwrap();
    let out2 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out2.status.success());
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        s2.contains("cache  discover:go"),
        "structural input should ignore function-body edits; got: {s2}"
    );

    // Add an import - matching lines change.
    std::fs::write(
        ws.join("src/main.go"),
        "package main\nimport \"fmt\"\nimport \"log\"\nfunc main() { fmt.Println(\"v3\") }\n",
    )
    .unwrap();
    let out3 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out3.status.success());
    let s3 = String::from_utf8_lossy(&out3.stdout);
    assert!(
        s3.contains("built  discover:go"),
        "import edit should invalidate structural input; got: {s3}"
    );
}

#[test]
fn structural_input_with_git_fast_path_warm_runs_are_consistent() {
    // Same property as the non-git test, exercised inside a real git
    // repo. Confirms the git fast-path produces the same hash as the
    // mtime walk (otherwise the cache key would shift on `git init`).
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(
        ws.join("src/main.go"),
        "package main\nimport \"fmt\"\nfunc main() { fmt.Println(\"v1\") }\n",
    )
    .unwrap();
    std::fs::write(ws.join("go.mod"), "module x\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: gitfastpath
cache:
  dir: ./cache
targets:
  - id: "discover:go"
    inputs:
      - "go.mod"
      - kind: structural
        files: "src/**/*.go"
        lines: ["package ", "import "]
    outputs: ["d.json"]
    command: 'echo "{}" > d.json'
"#,
    )
    .unwrap();

    // Initialize a real git repo and commit src/main.go so the fast-path
    // sees it as tracked.
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
    run_git(&["init", "--initial-branch=main"]);
    run_git(&["add", "."]);
    run_git(&["commit", "-m", "init"]);

    let out1 = Command::new(giant_bin()).arg("build").current_dir(ws).output().unwrap();
    assert!(out1.status.success(), "first build failed: {}", String::from_utf8_lossy(&out1.stderr));
    let s1 = String::from_utf8_lossy(&out1.stdout);
    assert!(s1.contains("built  discover:go"), "got: {s1}");

    // Edit function body - git status reports src/main.go as modified.
    // The fast-path should re-read it, see no matching-line change, and
    // emit the same fingerprint → cache hit on the discovery target.
    std::fs::write(
        ws.join("src/main.go"),
        "package main\nimport \"fmt\"\nfunc main() { fmt.Println(\"v2-very-different\") }\n",
    )
    .unwrap();
    let out2 = Command::new(giant_bin()).arg("build").current_dir(ws).output().unwrap();
    assert!(out2.status.success());
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        s2.contains("cache  discover:go"),
        "warm fast-path with function-body edit should cache-hit; got: {s2}"
    );

    // Edit import line - matching line changes → rebuild.
    std::fs::write(
        ws.join("src/main.go"),
        "package main\nimport \"fmt\"\nimport \"log\"\nfunc main() { fmt.Println(\"v3\") }\n",
    )
    .unwrap();
    let out3 = Command::new(giant_bin()).arg("build").current_dir(ws).output().unwrap();
    assert!(out3.status.success());
    let s3 = String::from_utf8_lossy(&out3.stdout);
    assert!(s3.contains("built  discover:go"), "import edit must rebuild; got: {s3}");
}

#[test]
fn fixture_discover_docker_runs_end_to_end() {
    // Copy the docker discovery fixture into a tempdir, run `giant build`,
    // verify the discovery emitted docker:api and docker:worker targets
    // and they ran (with safe echo placeholders, not real docker calls).
    if !have_program("jq") {
        eprintln!("skipping: jq not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    copy_dir(&fixture_path("discover-docker"), ws).unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn giant");
    assert!(
        out.status.success(),
        "build failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Bootstrap built the discovery target.
    assert!(
        stdout.contains("built  discover:docker") || stdout.contains("cache  discover:docker"),
        "expected discover:docker to run; got: {stdout}"
    );
    // Discovery emitted two docker targets - one per Dockerfile.
    assert!(stdout.contains("docker:api"), "expected docker:api in output; got: {stdout}");
    assert!(
        stdout.contains("docker:worker"),
        "expected docker:worker in output; got: {stdout}"
    );
}

#[test]
fn fixture_discover_go_runs_end_to_end() {
    // Copy the go discovery fixture into a tempdir; run `giant build`;
    // verify go:pkg:* targets get discovered and executed.
    if !have_program("go") || !have_program("jq") {
        eprintln!("skipping: go or jq not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    copy_dir(&fixture_path("discover-go"), ws).unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        // Isolate the Go build cache so test parallelism doesn't fight
        // over $XDG_CACHE_HOME/go-build.
        .env("GOCACHE", ws.join(".gocache").to_string_lossy().to_string())
        .env("GOMODCACHE", ws.join(".gomodcache").to_string_lossy().to_string())
        .output()
        .expect("spawn giant");
    assert!(
        out.status.success(),
        "build failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Bootstrap built (or cache-hit) the discovery target.
    assert!(
        stdout.contains("built  discover:go") || stdout.contains("cache  discover:go"),
        "expected discover:go to run; got: {stdout}"
    );
    // Discovery emitted two packages - library (pkg/util) + main (root).
    assert!(
        stdout.contains("go:pkg:pkg/util"),
        "expected go:pkg:pkg/util in output; got: {stdout}"
    );
    assert!(
        stdout.contains("go:pkg:root"),
        "expected go:pkg:root in output; got: {stdout}"
    );
    // The main package compiled into bin/root.
    assert!(
        ws.join("bin/root").exists(),
        "expected bin/root binary to exist after build"
    );
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
  - id: "a"
    inputs: ["src/a/**/*"]
    outputs: ["a.out"]
    command: "echo a > a.out"
  - id: "b"
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
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("built  a"), "a should build; got: {s}");
    assert!(!s.contains(" b "), "b should not appear; got: {s}");
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
  - id: "a"
    inputs: ["src/a/**/*"]
    outputs: ["a.out"]
    command: "echo a > a.out"
  - id: "b"
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
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("built  a"), "a should rebuild; got: {s}");
    assert!(!s.contains(" b "), "b should not appear; got: {s}");
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
  - id: "a"
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
    assert!(out.status.success(), "clean build with no changes should exit 0");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no affected"),
        "expected 'no affected' message; got: {stderr}"
    );
}

#[test]
fn affected_walks_downstream_transitively() {
    // a's output feeds b's input. Editing src/a/* should make BOTH a and
    // b run (b is transitively affected via output-based inference).
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
  - id: "a"
    inputs: ["src/a/**/*"]
    outputs: ["a.out"]
    command: "cp src/a/main.go a.out"
  - id: "b"
    inputs: ["a.out"]
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
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("built  a"), "a should build; got: {s}");
    assert!(s.contains("built  b"), "b should also build (transitive); got: {s}");
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
  - id: "demo"
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
  - id: "demo"
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
        std::fs::read_to_string(ws.join("out.txt"))
            .unwrap()
            .trim(),
        "second"
    );
}
