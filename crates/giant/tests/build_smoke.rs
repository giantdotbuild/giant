//! End-to-end smoke test: build a workspace twice; first is a cache miss,
//! second is a cache hit that restores outputs from CAS.

use std::process::Command;

fn giant_bin() -> std::path::PathBuf {
    // Cargo sets CARGO_BIN_EXE_<name> for the bin of the package under test.
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
        stdout.contains("FAIL") || stderr.contains("failed"),
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
    assert!(built(&s1, "a"), "expected a built; got: {s1}");
    assert!(built(&s1, "b"), "expected b built; got: {s1}");

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
        built(&s2, "a"),
        "expected a to rebuild (its input changed); got: {s2}"
    );
    assert!(
        cached(&s2, "b"),
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
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(built(&s, "a"), "a should build; got: {s}");
    assert!(built(&s, "b"), "b should build; got: {s}");
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
    outputs: [".giant/d/svc.json"]
    command: "./tools/discover.sh"
    scope: ["tools/"]
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
    assert!(
        out1.status.success(),
        "cold build failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let s1 = String::from_utf8_lossy(&out1.stdout);
    // discover:svc runs as a bootstrap target and the renderer hides
    // those - the proof it ran is that svc:hello (discovered) and
    // downstream (inferred dep on svc:hello) both built.
    assert!(built(&s1, "svc:hello"), "svc:hello should build; {s1}");
    assert!(built(&s1, "downstream"), "downstream should build; {s1}");
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
    assert!(cached(&s2, "svc:hello"));
    assert!(
        cached(&s2, "downstream"),
        "downstream must cache-hit on warm run (deterministic cache key); got: {s2}"
    );
}

#[test]
fn cooperative_discovery_writes_sidecar_on_cold_run() {
    // A discovery that emits a `reads` manifest in its output should
    // produce a sidecar file under .giant/discovery/ after the cold
    // run. The sidecar carries the targets, includes, and recorded
    // hashes used to verify the cached output on later runs.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir(ws.join("tools")).unwrap();
    std::fs::write(ws.join("go.mod"), "module example\n").unwrap();
    std::fs::write(
        ws.join("tools/discover.sh"),
        r#"#!/usr/bin/env bash
set -euo pipefail
mkdir -p .giant/d
cat > .giant/d/d.json <<'JSON'
{
  "targets": [],
  "reads": {
    "files": [
      { "path": "go.mod" }
    ]
  }
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
  name: cooperative
cache:
  dir: ./cache
include:
  - id: "discover:coop"
    outputs: [".giant/d/d.json"]
    command: "./tools/discover.sh"
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

    let sidecar_dir = ws.join(".giant/discovery");
    let entries: Vec<_> = std::fs::read_dir(&sidecar_dir)
        .unwrap_or_else(|e| panic!("sidecar dir missing at {}: {e}", sidecar_dir.display()))
        .filter_map(|r| r.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "expected one sidecar in {}, found {}",
        sidecar_dir.display(),
        entries.len()
    );

    let contents = std::fs::read_to_string(entries[0].path()).unwrap();
    assert!(
        contents.contains("\"path\":\"go.mod\""),
        "sidecar should record go.mod: {contents}"
    );
    assert!(
        contents.contains("\"content_hash\":"),
        "sidecar should carry a recorded hash: {contents}"
    );
}

#[test]
fn cooperative_discovery_cache_hits_on_warm_run() {
    // Cold run: discovery runs, sidecar is written, output file on disk.
    // Warm run with no filesystem changes: discovery does NOT execute its
    // command - proven by inserting a marker the script would overwrite
    // each run, and verifying the marker is the cold-run value.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir(ws.join("tools")).unwrap();
    std::fs::write(ws.join("go.mod"), "module example\n").unwrap();
    std::fs::write(
        ws.join("tools/discover.sh"),
        r#"#!/usr/bin/env bash
set -euo pipefail
mkdir -p .giant/d
date +%s%N > .giant/last-run.txt
cat > .giant/d/d.json <<'JSON'
{
  "targets": [],
  "reads": {
    "files": [
      { "path": "go.mod" }
    ]
  }
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
  name: warm
cache:
  dir: ./cache
include:
  - id: "discover:warm"
    outputs: [".giant/d/d.json"]
    command: "./tools/discover.sh"
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
        "cold build failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let marker1 = std::fs::read_to_string(ws.join(".giant/last-run.txt")).unwrap();
    assert!(!marker1.is_empty(), "cold run should have written marker");

    // Sanity: cold run should have written exactly one sidecar.
    let sidecar_dir = ws.join(".giant/discovery");
    let sidecars: Vec<_> = std::fs::read_dir(&sidecar_dir)
        .unwrap_or_else(|e| panic!("no sidecar dir after cold: {e}"))
        .filter_map(|r| r.ok())
        .collect();
    assert_eq!(sidecars.len(), 1, "expected one sidecar after cold");

    // Warm run: filesystem unchanged. The sidecar should verify and
    // the discovery command should not execute → the marker file is
    // not rewritten.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let out2 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out2.status.success());
    let marker2 = std::fs::read_to_string(ws.join(".giant/last-run.txt")).unwrap();
    assert_eq!(
        marker1,
        marker2,
        "warm run should not have re-executed the discovery (sidecar hit). cold stderr:\n{}\nwarm stderr:\n{}",
        String::from_utf8_lossy(&out1.stderr),
        String::from_utf8_lossy(&out2.stderr),
    );

    // Now change go.mod - the sidecar's recorded hash for go.mod no
    // longer matches, so discovery must re-run.
    std::fs::write(ws.join("go.mod"), "module example\n// changed\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let out3 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out3.status.success());
    let marker3 = std::fs::read_to_string(ws.join(".giant/last-run.txt")).unwrap();
    assert_ne!(
        marker1, marker3,
        "go.mod change should have invalidated the sidecar and re-run discovery"
    );
}

#[test]
fn strict_mode_errors_on_discovery_without_reads_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir(ws.join("tools")).unwrap();
    std::fs::write(
        ws.join("tools/discover.sh"),
        r#"#!/usr/bin/env bash
set -euo pipefail
mkdir -p .giant/d
echo '{"targets": []}' > .giant/d/d.json
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
  name: strict
cache:
  dir: ./cache
discovery:
  strict: true
include:
  - id: "discover:noreads"
    outputs: [".giant/d/d.json"]
    command: "./tools/discover.sh"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "strict mode should fail when discovery has no `reads`"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("discover:noreads") && stderr.contains("`reads`"),
        "expected strict-mode error naming the entry and `reads`: {stderr}"
    );
}

#[test]
fn non_cooperative_discovery_skips_sidecar_in_lenient_mode() {
    // A discovery without a `reads` manifest still works (lenient
    // default) but doesn't produce a sidecar.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir(ws.join("tools")).unwrap();
    std::fs::write(
        ws.join("tools/discover.sh"),
        r#"#!/usr/bin/env bash
set -euo pipefail
mkdir -p .giant/d
echo '{"targets": []}' > .giant/d/d.json
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
  name: noncoop
cache:
  dir: ./cache
include:
  - id: "discover:silent"
    outputs: [".giant/d/d.json"]
    command: "./tools/discover.sh"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out.status.success());

    let sidecar_dir = ws.join(".giant/discovery");
    let count = std::fs::read_dir(&sidecar_dir)
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(
        count, 0,
        "no sidecar should be written without a `reads` manifest"
    );
}

#[test]
fn discovery_can_emit_nested_includes() {
    // Wave-based recursive discovery (TDD-0003): a discovery target
    // emits another include target, which runs in the next wave and
    // emits a regular target. The final graph contains everything.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir(ws.join("tools")).unwrap();

    // wave-1 discovery: emits a wave-2 include target.
    std::fs::write(
        ws.join("tools/wave1.sh"),
        r#"#!/usr/bin/env bash
set -euo pipefail
mkdir -p .giant/d
cat > .giant/d/wave1.json <<'JSON'
{
  "include": [
    {
      "id": "discover:wave2",
      "outputs": [".giant/d/wave2.json"],
      "command": "./tools/wave2.sh",
      "scope": ["tools/"]
    }
  ]
}
JSON
"#,
    )
    .unwrap();

    // wave-2 discovery: emits a regular target.
    std::fs::write(
        ws.join("tools/wave2.sh"),
        r#"#!/usr/bin/env bash
set -euo pipefail
mkdir -p .giant/d
cat > .giant/d/wave2.json <<'JSON'
{
  "targets": [
    {
      "id": "deep:target",
      "inputs": [],
      "outputs": ["deep.txt"],
      "command": "echo from_deep_discovery > deep.txt"
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
        for script in ["tools/wave1.sh", "tools/wave2.sh"] {
            let p = std::fs::metadata(ws.join(script)).unwrap().permissions();
            let mut p = p;
            p.set_mode(0o755);
            std::fs::set_permissions(ws.join(script), p).unwrap();
        }
    }

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: recursive-discovery
cache:
  dir: ./cache
include:
  - id: "discover:wave1"
    outputs: [".giant/d/wave1.json"]
    command: "./tools/wave1.sh"
    scope: ["tools/"]
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
        "build failed: {}\n--- stdout ---\n{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout),
    );
    let s = String::from_utf8_lossy(&out.stdout);
    // The wave-2 discovery's emitted target should have built - proof
    // that recursive discovery is working end-to-end.
    assert!(
        built(&s, "deep:target"),
        "deep:target (from wave-2 discovery) should build; output:\n{s}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("deep.txt")).unwrap().trim(),
        "from_deep_discovery"
    );
}

#[test]
fn discovery_cycle_is_detected() {
    // A discovery target that emits an include pointing at itself
    // would loop forever. The seen-set in the wave loop should
    // dedupe it, so the build completes normally rather than
    // hanging.
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir(ws.join("tools")).unwrap();
    std::fs::write(
        ws.join("tools/self-emit.sh"),
        r#"#!/usr/bin/env bash
set -euo pipefail
mkdir -p .giant/d
cat > .giant/d/loop.json <<'JSON'
{
  "include": [
    {
      "id": "discover:loop",
      "outputs": [".giant/d/loop.json"],
      "command": "./tools/self-emit.sh",
      "scope": ["tools/"]
    }
  ],
  "targets": [
    {
      "id": "loop:result",
      "inputs": [],
      "outputs": ["loop.txt"],
      "command": "echo loop_resolved > loop.txt"
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
        let p = std::fs::metadata(ws.join("tools/self-emit.sh"))
            .unwrap()
            .permissions();
        let mut p = p;
        p.set_mode(0o755);
        std::fs::set_permissions(ws.join("tools/self-emit.sh"), p).unwrap();
    }

    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: cycle-discovery
cache:
  dir: ./cache
include:
  - id: "discover:loop"
    outputs: [".giant/d/loop.json"]
    command: "./tools/self-emit.sh"
    scope: ["tools/"]
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
        "build should complete (cycle deduped): {}",
        String::from_utf8_lossy(&out.stderr),
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        built(&s, "loop:result"),
        "non-cyclic target should still build; {s}"
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
    assert!(built(&s, "docker:img"), "expected build to run; got: {s}");
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
    assert!(
        out1.status.success(),
        "first build failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let s1 = String::from_utf8_lossy(&out1.stdout);
    assert!(built(&s1, "discover:go"), "got: {s1}");

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
        cached(&s2, "discover:go"),
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
        built(&s3, "discover:go"),
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
    assert!(built(&s1, "discover:go"), "got: {s1}");

    // Edit function body - git status reports src/main.go as modified.
    // The fast-path should re-read it, see no matching-line change, and
    // emit the same fingerprint → cache hit on the discovery target.
    std::fs::write(
        ws.join("src/main.go"),
        "package main\nimport \"fmt\"\nfunc main() { fmt.Println(\"v2-very-different\") }\n",
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
        cached(&s2, "discover:go"),
        "warm fast-path with function-body edit should cache-hit; got: {s2}"
    );

    // Edit import line - matching line changes → rebuild.
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
        built(&s3, "discover:go"),
        "import edit must rebuild; got: {s3}"
    );
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

    // Bootstrap output is hidden by the renderer; the proof discovery
    // ran is that the two docker:* targets it emitted show up below.
    assert!(
        stdout.contains("docker:api"),
        "expected docker:api in output; got: {stdout}"
    );
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
        .env(
            "GOMODCACHE",
            ws.join(".gomodcache").to_string_lossy().to_string(),
        )
        .output()
        .expect("spawn giant");
    assert!(
        out.status.success(),
        "build failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Bootstrap output is hidden by the renderer; discovery's proof is
    // that the two go:pkg targets below show up.
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

    // The discovery emitted a `reads` manifest, so the engine should
    // have written a sidecar under .giant/discovery/. (TDD-0015.)
    let sidecar_count = std::fs::read_dir(ws.join(".giant/discovery"))
        .map(|d| d.count())
        .unwrap_or(0);
    assert_eq!(
        sidecar_count, 1,
        "discover-go.sh emits a reads manifest, expected one sidecar"
    );
}

#[test]
fn fixture_discover_go_warm_skips_discovery_when_nothing_changed() {
    // Cooperative discovery: cold run writes a sidecar; warm run with no
    // filesystem changes should not re-execute discover-go.sh. We prove
    // it by capturing the sidecar's mtime - if discovery re-ran, the
    // sidecar would be rewritten.
    if !have_program("go") || !have_program("jq") {
        eprintln!("skipping: go or jq not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    copy_dir(&fixture_path("discover-go"), ws).unwrap();

    let env_pairs = [
        (
            "GOCACHE",
            ws.join(".gocache").to_string_lossy().into_owned(),
        ),
        (
            "GOMODCACHE",
            ws.join(".gomodcache").to_string_lossy().into_owned(),
        ),
    ];

    let mut cmd1 = Command::new(giant_bin());
    cmd1.arg("build").current_dir(ws);
    for (k, v) in &env_pairs {
        cmd1.env(k, v);
    }
    let out1 = cmd1.output().expect("spawn giant");
    assert!(
        out1.status.success(),
        "cold build failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );

    let sidecar_dir = ws.join(".giant/discovery");
    let sidecar_path = std::fs::read_dir(&sidecar_dir)
        .expect("sidecar dir exists")
        .next()
        .expect("at least one sidecar")
        .expect("readable dir entry")
        .path();
    let mtime1 = std::fs::metadata(&sidecar_path)
        .unwrap()
        .modified()
        .unwrap();

    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut cmd2 = Command::new(giant_bin());
    cmd2.arg("build").current_dir(ws);
    for (k, v) in &env_pairs {
        cmd2.env(k, v);
    }
    let out2 = cmd2.output().expect("spawn giant");
    assert!(out2.status.success());

    let mtime2 = std::fs::metadata(&sidecar_path)
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        mtime1, mtime2,
        "sidecar should not be rewritten on warm run (discovery skipped)"
    );

    // Now edit pkg/util/util.go's body without touching package/import
    // lines. The excerpt verifier should report Match → still no rerun.
    let util_path = ws.join("pkg/util/util.go");
    let before = std::fs::read_to_string(&util_path).unwrap();
    let edited = before.replace(
        "func Greet",
        "// inserted comment unrelated to package/import\nfunc Greet",
    );
    assert_ne!(before, edited, "the replace should have changed the file");
    std::fs::write(&util_path, &edited).unwrap();

    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut cmd3 = Command::new(giant_bin());
    cmd3.arg("build").current_dir(ws);
    for (k, v) in &env_pairs {
        cmd3.env(k, v);
    }
    let out3 = cmd3.output().expect("spawn giant");
    assert!(out3.status.success());

    let mtime3 = std::fs::metadata(&sidecar_path)
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        mtime1, mtime3,
        "function-body edits shouldn't invalidate the discovery sidecar"
    );

    // Editing go.mod (a whole-file entry in reads) should re-run.
    std::fs::write(
        ws.join("go.mod"),
        "module example.com/giantfixture\n\ngo 1.21\n",
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut cmd4 = Command::new(giant_bin());
    cmd4.arg("build").current_dir(ws);
    for (k, v) in &env_pairs {
        cmd4.env(k, v);
    }
    let out4 = cmd4.output().expect("spawn giant");
    assert!(out4.status.success());

    let mtime4 = std::fs::metadata(&sidecar_path)
        .unwrap()
        .modified()
        .unwrap();
    assert_ne!(
        mtime1, mtime4,
        "go.mod edit should have invalidated the sidecar and re-run discovery"
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
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(built(&s, "a"), "a should build; got: {s}");
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
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(built(&s, "a"), "a should rebuild; got: {s}");
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
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(built(&s, "a"), "a should build; got: {s}");
    assert!(built(&s, "b"), "b should also build (transitive); got: {s}");
}

#[test]
fn affected_subcommand_lists_targets_without_building() {
    // `giant affected --file <path>` should print sorted target IDs and
    // NOT actually run any commands. We assert no output files appeared.
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
  name: aff_cmd
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
  - id: "c"
    inputs: ["a.out"]
    outputs: ["c.out"]
    command: "cp a.out c.out"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .args(["affected", "--file", "src/a/main.go"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "affected failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    // Sorted: a, c. (b isn't affected; b's input glob doesn't match src/a/**.)
    assert_eq!(
        lines,
        vec!["a", "c"],
        "expected just 'a' and 'c'; got: {stdout}"
    );
    // Nothing was built.
    assert!(
        !ws.join("a.out").exists(),
        "a.out must not exist - affected shouldn't build"
    );
    assert!(!ws.join("c.out").exists(), "c.out must not exist");
}

#[test]
fn affected_subcommand_empty_is_clean_exit() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::create_dir_all(ws.join("src")).unwrap();
    std::fs::write(ws.join("src/main.go"), "package main\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: aff_empty
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

    // --file pointing at something that matches no target.
    let out = Command::new(giant_bin())
        .args(["affected", "--file", "README.md"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "affected with no matches should exit 0"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.trim().is_empty(),
        "expected empty stdout; got: {stdout:?}"
    );
}

#[test]
fn explain_subcommand_shows_cache_miss_then_hit() {
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
  - id: "demo"
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
    let out1 = Command::new(giant_bin())
        .args(["explain", "demo"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out1.status.success(),
        "explain failed: {}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let s1 = String::from_utf8_lossy(&out1.stdout);
    assert!(
        s1.contains("target:      demo"),
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
    assert!(
        s1.contains("in.txt"),
        "in.txt should be listed as input; got: {s1}"
    );

    // Build, then re-explain: should report HIT with outputs metadata.
    let build = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(build.status.success());

    let out2 = Command::new(giant_bin())
        .args(["explain", "demo"])
        .current_dir(ws)
        .output()
        .unwrap();
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
fn explain_unknown_target_errors() {
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
  - id: "real"
    inputs: []
    outputs: ["x"]
    command: "true"
"#,
    )
    .unwrap();
    let out = Command::new(giant_bin())
        .args(["explain", "ghost"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(!out.status.success(), "unknown target should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ghost"),
        "stderr should name the target; got: {stderr}"
    );
}

#[test]
fn graph_subcommand_lists_all_targets() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: graph_list
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
    command: "cat a.txt > b.txt"
  - id: "c"
    inputs: []
    outputs: ["c.txt"]
    command: "echo c > c.txt"
"#,
    )
    .unwrap();
    let out = Command::new(giant_bin())
        .arg("graph")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "graph failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    // All three IDs present.
    for id in ["a", "b", "c"] {
        assert!(s.contains(id), "expected target {id:?} in list; got: {s}");
    }
    // 'b' depends on 'a' (inferred via a.txt), so it should show the arrow.
    assert!(s.contains("b") && s.contains("→") && s.contains("a"));
    // Footer.
    assert!(s.contains("3 target(s)"), "expected count footer; got: {s}");
}

#[test]
fn graph_subcommand_shows_tree() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: graph_tree
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
    command: "cat a.txt > b.txt"
  - id: "c"
    inputs: ["b.txt"]
    outputs: ["c.txt"]
    command: "cat b.txt > c.txt"
"#,
    )
    .unwrap();

    // Tree under 'c' should be c → b → a.
    let out = Command::new(giant_bin())
        .args(["graph", "c"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    // Order matters: first line is 'c', then indented 'b', then deeper-indented 'a'.
    let lines: Vec<&str> = s.lines().collect();
    assert_eq!(lines[0].trim(), "c");
    assert!(lines[1].starts_with("  ") && lines[1].contains("b"));
    assert!(lines[2].starts_with("    ") && lines[2].contains("a"));
}

#[test]
fn graph_subcommand_reverse_shows_downstream() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: graph_rev
cache:
  dir: ./cache
targets:
  - id: "lib"
    inputs: []
    outputs: ["lib.o"]
    command: "echo lib > lib.o"
  - id: "app"
    inputs: ["lib.o"]
    outputs: ["app"]
    command: "cp lib.o app"
  - id: "release"
    inputs: ["app"]
    outputs: ["release.tag"]
    command: "echo r > release.tag"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .args(["graph", "lib", "--reverse"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    // lib → app → release downstream
    assert!(s.contains("lib"), "got: {s}");
    assert!(s.contains("app"), "got: {s}");
    assert!(s.contains("release"), "got: {s}");
}

#[test]
fn graph_unknown_target_errors() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: graph_unknown
cache:
  dir: ./cache
targets:
  - id: "real"
    inputs: []
    outputs: ["x"]
    command: "true"
"#,
    )
    .unwrap();
    let out = Command::new(giant_bin())
        .args(["graph", "ghost"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("ghost"));
}

#[test]
fn clean_removes_cache_contents() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: clean_test
cache:
  dir: ./cache
targets:
  - id: "demo"
    inputs: []
    outputs: ["out.txt"]
    command: "echo hello > out.txt"
"#,
    )
    .unwrap();

    // Build to populate the cache.
    let build = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(build.status.success());
    assert!(
        ws.join("cache/version").exists(),
        "cache should have been initialised"
    );

    // Dry-run: shows summary, doesn't delete.
    let dry = Command::new(giant_bin())
        .args(["clean", "--dry-run"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(dry.status.success());
    let s = String::from_utf8_lossy(&dry.stdout);
    assert!(
        s.contains("entries:"),
        "summary should show entries; got: {s}"
    );
    assert!(s.contains("Dry run"), "should label as dry run; got: {s}");
    // AC entries should still be on disk.
    let ac_count_after_dry = walkdir::WalkDir::new(ws.join("cache/ac"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .count();
    assert!(ac_count_after_dry > 0, "dry-run must not delete AC entries");

    // Real clean.
    let clean = Command::new(giant_bin())
        .args(["clean", "-y"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        clean.status.success(),
        "clean failed: {}",
        String::from_utf8_lossy(&clean.stderr)
    );
    let s = String::from_utf8_lossy(&clean.stdout);
    assert!(
        s.contains("Cleared"),
        "expected 'Cleared' message; got: {s}"
    );

    // After clean: cache dir exists but is empty (no AC, CAS, etc.).
    let after_count = walkdir::WalkDir::new(ws.join("cache"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .count();
    assert_eq!(after_count, 0, "cache should be empty after clean");
}

#[test]
fn clean_empty_cache_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: clean_empty
cache:
  dir: ./cache
targets:
  - id: "x"
    inputs: []
    outputs: ["x"]
    command: "echo x > x"
"#,
    )
    .unwrap();
    // Cache directory doesn't exist yet (no build run).
    let out = Command::new(giant_bin())
        .args(["clean", "-y"])
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(out.status.success(), "clean on missing cache should exit 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("empty") || s.contains("Nothing to clean"),
        "expected friendly empty message; got: {s}"
    );
}

#[test]
fn clean_requires_yes_in_non_interactive() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: clean_y
cache:
  dir: ./cache
targets:
  - id: "demo"
    inputs: []
    outputs: ["out.txt"]
    command: "echo hi > out.txt"
"#,
    )
    .unwrap();
    // Populate cache.
    let _ = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .unwrap();

    // Run with stdin closed (Command::output gives no tty by default) and no -y.
    let out = Command::new(giant_bin())
        .arg("clean")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        !out.status.success(),
        "clean without -y in non-interactive should fail"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("pass -y") || stderr.contains("not a terminal"),
        "expected guidance to pass -y; got stderr: {stderr}"
    );
    // Cache must still be intact.
    assert!(ws.join("cache/version").exists());
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
  - id: "demo"
    inputs: ["src/in.txt"]
    outputs: ["out.txt"]
    command: "cp src/in.txt out.txt"
"#,
    )
    .unwrap();

    let mut child = Command::new(giant_bin())
        .arg("watch")
        .current_dir(ws)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn giant watch");
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
        line_has(&buf, "BUILD", "demo"),
        "expected initial build of demo in watch output; got: {buf}"
    );
}

#[test]
#[cfg(unix)]
fn watch_respects_pattern_selection() {
    // Spawn `giant watch go:bin:*` against a fixture with both
    // `go:bin:server` (matches) and `go:lib:util` (excluded). Editing
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
  - id: "go:bin:server"
    inputs: ["src/bin.txt"]
    outputs: ["bin.out"]
    command: "cp src/bin.txt bin.out"
  - id: "go:lib:util"
    inputs: ["src/lib.txt"]
    outputs: ["lib.out"]
    command: "cp src/lib.txt lib.out"
"#,
    )
    .unwrap();

    let mut child = Command::new(giant_bin())
        .args(["watch", "go:bin:*"])
        .current_dir(ws)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn giant watch");
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
        "lib.out should not be built - go:lib:util is outside the selection"
    );

    // Edit the lib's input. The watcher will see the change and run a
    // cycle, but the pattern filter must drop go:lib:util from the
    // selection, leaving no targets to build.
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
    // the filter in place - otherwise we'd see a BUILD for go:lib:util.
    assert!(
        buf.contains("no targets affected"),
        "expected a 'no targets affected' cycle after editing the excluded lib input; got: {buf}"
    );
    assert!(
        !line_has(&buf, "BUILD", "go:lib:util"),
        "go:lib:util must never appear as built in watch output; got: {buf}"
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
  - id: "demo:logs"
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
        cached(&s2, "demo:logs"),
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
  - id: "demo:noreplay"
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
        cached(&s2, "demo:noreplay"),
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
  - id: "demo:nocapture"
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
    assert!(cached(&s2, "demo:nocapture"), "got: {s2}");
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
  - id: "lint"
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
  - id: "slow"
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
