//! End-to-end exercise of giant-task: parse giant.yaml, spawn `giant build`
//! for deps, run the task command, propagate exit code.
//!
//! Uses Cargo's `CARGO_BIN_EXE_*` env vars to find both binaries in
//! the workspace target/ dir without relying on PATH order.

use std::process::Command;

fn giant_task_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant-task"))
}

/// Path to the sibling `giant` binary. Cargo doesn't expose
/// `CARGO_BIN_EXE_giant` from a different package's tests, so we
/// derive it from giant-task's own binary path (same target dir).
fn giant_bin() -> std::path::PathBuf {
    let mut p = giant_task_bin();
    p.set_file_name("giant");
    p
}

fn write_config(dir: &std::path::Path, yaml: &str) {
    std::fs::write(dir.join("giant.yaml"), yaml).unwrap();
}

#[test]
fn list_subcommand_prints_tasks_and_workspace() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: smoke }
tasks:
  hello:
    command: "echo hi"
    description: "say hello"
  deploy:
    command: "kubectl"
    description: "deploy the thing"
"#,
    );

    let out = Command::new(giant_task_bin())
        .arg("--list")
        .current_dir(dir.path())
        .output()
        .expect("spawn giant-task");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("hello"), "missing hello: {s}");
    assert!(s.contains("deploy"), "missing deploy: {s}");
    assert!(s.contains("smoke"), "workspace name missing: {s}");
}

#[test]
fn empty_invocation_prints_list_with_zero_exit() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: empty }
tasks:
  ping:
    command: "echo pong"
"#,
    );
    let out = Command::new(giant_task_bin())
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("ping"));
}

#[test]
fn runs_simple_task_command() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: run }
tasks:
  marker:
    command: "echo hello > marker.txt"
"#,
    );
    let out = Command::new(giant_task_bin())
        .arg("marker")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(dir.path().join("marker.txt")).unwrap();
    assert_eq!(body.trim(), "hello");
}

#[test]
fn unknown_task_errors_helpfully() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: u }
tasks:
  yes:
    command: "echo yes"
"#,
    );
    let out = Command::new(giant_task_bin())
        .arg("nope")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no task named 'nope'"), "got: {stderr}");
}

#[test]
fn task_args_become_env_vars() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: args }
tasks:
  deploy:
    command: "echo $GIANT_ARG_ENV > out.txt"
    args:
      env:
        default: "staging"
        choices: ["staging", "prod"]
"#,
    );

    // Default is applied when --arg is omitted.
    let out = Command::new(giant_task_bin())
        .arg("deploy")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt"))
            .unwrap()
            .trim(),
        "staging"
    );

    // --arg overrides the default; the choices list permits "prod".
    let out = Command::new(giant_task_bin())
        .args(["deploy", "--arg", "env=prod"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt"))
            .unwrap()
            .trim(),
        "prod"
    );

    // Value outside choices is rejected.
    let out = Command::new(giant_task_bin())
        .args(["deploy", "--arg", "env=lol"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not one of"));
}

#[test]
fn passthrough_args_appear_as_positional_in_sh() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: pt }
tasks:
  echo:
    command: "echo \"$1 $2\" > out.txt"
"#,
    );
    let out = Command::new(giant_task_bin())
        .args(["echo", "--", "alpha", "beta"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt"))
            .unwrap()
            .trim(),
        "alpha beta"
    );
}

#[test]
fn deps_drive_giant_build_then_command_runs() {
    // Real end-to-end: the task declares a target dep, giant-task
    // spawns `giant build <target>` (via GIANT_TASK_BUILD_BIN pointing
    // at the workspace's giant binary), the target produces a file,
    // then the task's command reads it.
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: deps }
cache:
  dir: ./cache
targets:
  - id: "make:input"
    inputs: []
    outputs: ["input.txt"]
    command: "echo deps-output > input.txt"
tasks:
  combine:
    command: "cat input.txt > result.txt && echo done >> result.txt"
    deps: ["make:input"]
"#,
    );

    let out = Command::new(giant_task_bin())
        .env("GIANT_TASK_BUILD_BIN", giant_bin())
        .arg("combine")
        .current_dir(dir.path())
        .output()
        .expect("spawn giant-task");
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    let body = std::fs::read_to_string(dir.path().join("result.txt")).unwrap();
    assert_eq!(body.trim(), "deps-output\ndone");
}

#[test]
fn finally_runs_even_when_command_fails() {
    // The task's command fails. The `finally` block should still run
    // and create the marker file. Exit code propagates from command,
    // not finally.
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: fin }
tasks:
  main:
    command: "exit 7"
    finally: ["cleanup"]
  cleanup:
    command: "echo cleaned > finally-ran.txt"
"#,
    );
    let out = Command::new(giant_task_bin())
        .arg("main")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "main should fail");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("finally-ran.txt"))
            .unwrap()
            .trim(),
        "cleaned",
        "finally must have run despite command failure"
    );
}

#[test]
fn needs_run_in_declared_order_before_command() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: needs }
tasks:
  main:
    command: "echo main >> order.txt"
    needs: ["one", "two"]
  one:
    command: "echo one >> order.txt"
  two:
    command: "echo two >> order.txt"
"#,
    );
    let out = Command::new(giant_task_bin())
        .arg("main")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(dir.path().join("order.txt")).unwrap();
    assert_eq!(body, "one\ntwo\nmain\n");
}

#[test]
fn need_failure_skips_command_but_runs_finally() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: nf }
tasks:
  main:
    command: "echo SHOULD_NOT_RUN > main-marker.txt"
    needs: ["broken"]
    finally: ["cleanup"]
  broken:
    command: "exit 1"
  cleanup:
    command: "echo cleaned > finally-marker.txt"
"#,
    );
    let out = Command::new(giant_task_bin())
        .arg("main")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        !dir.path().join("main-marker.txt").exists(),
        "main command must not have run when its need failed"
    );
    assert!(
        dir.path().join("finally-marker.txt").exists(),
        "finally must still have run"
    );
}

#[test]
#[cfg(unix)]
fn services_start_then_task_runs_then_services_stop() {
    // 1. The service writes its own PID + a sentinel file on startup.
    // 2. The task runs only after the service is ready and verifies
    //    the sentinel + reads the PID.
    // 3. After the task ends, the supervisor kills the service. We
    //    poll the PID with `kill -0` for a brief settle window and
    //    assert it's gone.
    let dir = tempfile::tempdir().unwrap();
    let ready = dir.path().join("svc-ready");
    let pid_file = dir.path().join("svc.pid");
    let ready_s = ready.display().to_string();
    let pid_s = pid_file.display().to_string();
    write_config(
        dir.path(),
        &format!(
            r#"
workspace: {{ name: svc }}
services:
  fake:
    command: 'echo $$ > {pid}; touch {ready}; exec sleep 30'
    ready:
      command: 'test -f {ready}'
      period_secs: 1
      timeout_secs: 5
tasks:
  use_service:
    command: 'test -f {ready} && echo ok > task-ok.txt'
    services: ["fake"]
"#,
            ready = ready_s,
            pid = pid_s,
        ),
    );
    let out = Command::new(giant_task_bin())
        .arg("use_service")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("task-ok.txt"))
            .unwrap()
            .trim(),
        "ok",
        "task body must have seen the service ready"
    );
    // Service must actually be dead afterward.
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // Poll briefly: tokio-process-tools sends SIGINT, then SIGTERM
    // after 2s, then SIGKILL - give the worst case a second of
    // headroom past the initial signals to settle.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(6);
    loop {
        // `kill -0 <pid>` returns 0 if alive, non-zero if not.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        if !alive {
            return; // success
        }
        if std::time::Instant::now() >= deadline {
            panic!("service pid {pid} still alive after task ended + 6s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
fn service_that_never_becomes_ready_fails_clean() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: nr }
services:
  never:
    command: "exec sleep 30"
    ready:
      command: "test -f /never-exists/nope"
      period_secs: 1
      timeout_secs: 1
tasks:
  use_it:
    command: "echo SHOULD_NOT_RUN > main.txt"
    services: ["never"]
"#,
    );
    let out = Command::new(giant_task_bin())
        .arg("use_it")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        !dir.path().join("main.txt").exists(),
        "task command must not run if service didn't become ready"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("didn't become ready"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn config_validation_rejects_unknown_service_reference() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: bad }
tasks:
  ghost:
    command: "true"
    services: ["does-not-exist"]
"#,
    );
    let out = Command::new(giant_task_bin())
        .arg("--list")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such service"));
}

#[test]
fn dep_failure_propagates_and_skips_command() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: badfail }
cache:
  dir: ./cache
targets:
  - id: "always:fail"
    inputs: []
    outputs: []
    cache: false
    command: "exit 7"
tasks:
  use_it:
    command: "echo SHOULD_NOT_RUN > marker.txt"
    deps: ["always:fail"]
"#,
    );
    let out = Command::new(giant_task_bin())
        .env("GIANT_TASK_BUILD_BIN", giant_bin())
        .arg("use_it")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        !dir.path().join("marker.txt").exists(),
        "task command must not have run when its dep failed"
    );
}

#[test]
fn reserved_task_name_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: r }
tasks:
  build:
    command: "echo no"
"#,
    );
    let out = Command::new(giant_task_bin())
        .arg("--list")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("shadows a built-in"));
}

#[test]
fn no_config_in_tree_errors() {
    let dir = tempfile::tempdir().unwrap();
    // No giant.yaml - find_config walks up to /, doesn't find one,
    // surfaces a clear error.
    let out = Command::new(giant_task_bin())
        .arg("--list")
        .current_dir(dir.path())
        .output()
        .unwrap();
    // Note: this can spuriously succeed if some ancestor of the
    // tempdir HAS a giant.yaml. The runner uses CARGO_TARGET_TMPDIR
    // by default on macOS / linux, which is under target/ inside the
    // workspace - and the workspace root does NOT have a giant.yaml,
    // so we're safe.
    assert!(
        !out.status.success(),
        "expected failure when no giant.yaml exists upward; got stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
}
