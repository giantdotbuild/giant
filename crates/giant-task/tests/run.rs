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
    command: "echo $GIANT_ARG_ENV $env > out.txt"
    args:
      - name: env
        default: "staging"
        choices: ["staging", "prod"]
"#,
    );

    // Default is applied when no value is given. Both bindings are set:
    // GIANT_ARG_ENV and the plain $env.
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
        "staging staging"
    );

    // A positional value binds to the first declared arg.
    let out = Command::new(giant_task_bin())
        .args(["deploy", "prod"])
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
        "prod prod"
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
        "prod prod"
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
fn flaglike_args_pass_through_to_variadic_without_dashdash() {
    // The user's `giant deploy --force` case: a variadic arg forwards
    // everything - including flag-like values - to the command as $@,
    // with no `--` needed.
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: pt }
tasks:
  echo:
    command: 'echo "$@" > out.txt'
    args:
      - name: rest
        variadic: true
"#,
    );
    let out = Command::new(giant_task_bin())
        .args(["echo", "--release", "--nocapture", "x"])
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
        "--release --nocapture x"
    );
}

#[test]
fn per_task_help_prints_the_signature() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: h }
tasks:
  deploy:
    description: "deploy the app"
    command: "true"
    args:
      - name: env
        choices: ["staging", "prod"]
        description: "target environment"
      - name: tag
        default: "latest"
"#,
    );
    let out = Command::new(giant_task_bin())
        .args(["deploy", "--help"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("usage: giant deploy <env> [tag=latest]"),
        "expected the task signature; got: {s}"
    );
    assert!(s.contains("staging|prod"), "expected choices; got: {s}");
}

#[test]
fn bare_help_shows_giant_task_help() {
    // `giant task --help` (no task name) → giant-task's own help, via clap.
    let out = Command::new(giant_task_bin())
        .arg("--help")
        .output()
        .unwrap();
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("Task-runner porcelain") || s.contains("Usage:"),
        "expected giant-task's general help; got: {s}"
    );
}

#[test]
fn args_after_dashdash_forward_to_the_command_without_declared_args() {
    // The user's `giant task docs-preview -- --host` case: a task with NO
    // declared args still forwards everything after `--` to the command as
    // `$@`, verbatim (flags included, no stray `--`).
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: pt }
tasks:
  wrap:
    command: 'printf "%s\n" "$@" > out.txt'
"#,
    );
    let out = Command::new(giant_task_bin())
        .args(["wrap", "--", "--host", "0.0.0.0", "--port=4321"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "--host\n0.0.0.0\n--port=4321\n"
    );
}

#[test]
fn bare_args_forward_when_the_task_declares_none() {
    // The user's `giant task docs-preview --host` case: a task with no declared
    // args is a pass-through wrapper, so bare args (flags included) reach the
    // command as `$@` without `--` or a variadic declaration.
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: pt }
tasks:
  wrap:
    command: 'printf "%s\n" "$@" > out.txt'
"#,
    );
    let out = Command::new(giant_task_bin())
        .args(["wrap", "--host", "0.0.0.0"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "--host\n0.0.0.0\n"
    );
}

#[test]
fn variadic_arg_becomes_positional_params() {
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: var }
tasks:
  many:
    command: "echo \"$# $@\" > out.txt"
    args:
      - name: rest
        variadic: true
"#,
    );
    let out = Command::new(giant_task_bin())
        .args(["many", "a", "b", "c"])
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
        "3 a b c"
    );
}

#[test]
fn shebang_body_runs_as_a_script() {
    let dir = tempfile::tempdir().unwrap();
    // A `#!` body is written to a temp file and exec'd directly, so `$0`
    // is that temp script (not `sh`). That proves shebang dispatch.
    write_config(
        dir.path(),
        "workspace: { name: sheb }\ntasks:\n  sheb:\n    command: |\n      #!/bin/sh\n      echo \"$0\" > out.txt\n",
    );
    let out = Command::new(giant_task_bin())
        .arg("sheb")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let arg0 = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
    assert!(
        arg0.contains("giant-task-"),
        "expected the body to run from a temp script; $0 was: {arg0}"
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
  - name: "input"
    inputs: []
    outputs: ["input.txt"]
    command: "echo deps-output > input.txt"
tasks:
  combine:
    command: "cat input.txt > result.txt && echo done >> result.txt"
    deps: ["//:input"]
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
  - name: "fail"
    inputs: []
    outputs: []
    cache: false
    command: "exit 7"
tasks:
  use_it:
    command: "echo SHOULD_NOT_RUN > marker.txt"
    deps: ["//:fail"]
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
fn task_named_after_a_giant_command_is_allowed() {
    // Tasks are reached only as `giant task <name>` (ADR-0035), so a task named
    // `build` is unambiguous and loads fine - it does not collide with the
    // `giant build` porcelain.
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: r }
tasks:
  build:
    command: "echo ok"
"#,
    );
    let out = Command::new(giant_task_bin())
        .arg("--list")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("build"));
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

#[test]
fn supervise_mode_returns_when_a_service_exits() {
    // A task with `services:` and no `command:` supervises in the
    // foreground; it returns when a service exits (here, quickly).
    let dir = tempfile::tempdir().unwrap();
    write_config(
        dir.path(),
        r#"
workspace: { name: sup }
services:
  quick:
    command: "sleep 0.3"
tasks:
  dev:
    services: ["quick"]
"#,
    );
    let started = std::time::Instant::now();
    let out = Command::new(giant_task_bin())
        .arg("dev")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "supervise should return on the service exit, not hang"
    );
}

#[test]
fn services_start_in_dependency_order() {
    // `api` needs `db`; the supervisor must bring `db` to ready before
    // starting `api`. db touches a marker only after a delay + its ready
    // probe; api (started after db is ready) sees the marker and proves
    // ordering by touching its own. api then exits, ending the supervise.
    let dir = tempfile::tempdir().unwrap();
    let db_ready = dir.path().join("db-ready");
    let api_ok = dir.path().join("api-ok");
    write_config(
        dir.path(),
        &format!(
            r#"
workspace: {{ name: ord }}
services:
  db:
    command: 'sleep 0.2; touch {db}; exec sleep 30'
    ready:
      command: 'test -f {db}'
      period_secs: 1
      timeout_secs: 5
  api:
    needs: ["db"]
    command: 'test -f {db} && touch {ok}'
tasks:
  dev:
    services: ["api"]
"#,
            db = db_ready.display(),
            ok = api_ok.display(),
        ),
    );
    let out = Command::new(giant_task_bin())
        .arg("dev")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        api_ok.exists(),
        "api must have started after db became ready (saw the db marker)"
    );
}

/// A signal sent straight to giant-task (the `pkill -INT` / `systemctl
/// stop` case, not a terminal Ctrl-C) must still run `finally`. This is
/// the bug: with no handler the process died before cleanup.
#[test]
fn sigterm_runs_finally() {
    use std::time::{Duration, Instant};

    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    let cleaned = dir.path().join("cleaned");
    write_config(
        dir.path(),
        &format!(
            r#"
workspace: {{ name: sig }}
tasks:
  long:
    command: 'touch {started}; exec sleep 30'
    finally: ["cleanup"]
  cleanup:
    command: 'touch {cleaned}'
"#,
            started = started.display(),
            cleaned = cleaned.display(),
        ),
    );

    // Spawn it running (don't `.output()` - we need to signal it mid-run).
    let mut child = Command::new(giant_task_bin())
        .arg("long")
        .current_dir(dir.path())
        .spawn()
        .unwrap();

    // Wait until the command body is actually executing.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !started.exists() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("task command never started");
        }
        std::thread::sleep(Duration::from_millis(25));
    }

    // SIGTERM the giant-task process directly - no foreground process
    // group delivering it to the child for us.
    let pid = child.id() as i32;
    assert_eq!(
        unsafe { libc::kill(pid, libc::SIGTERM) },
        0,
        "failed to signal giant-task"
    );

    // It must exit, and `finally` must have run.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("giant-task did not exit after SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(25));
    };

    assert!(
        cleaned.exists(),
        "finally cleanup must run when giant-task is signalled directly"
    );
    // Conventional 128 + SIGTERM(15).
    assert_eq!(status.code(), Some(143), "expected 128 + SIGTERM exit code");
}

/// The whole point of ADR-0022/TDD-0019: `--watch` reruns the task when a
/// *dependency's* source changes, not just the task's own `inputs:`. The
/// engine session expands `deps:` through the graph; the porcelain only
/// subscribes and reacts. End-to-end through a real `giant session`.
#[test]
fn watch_reruns_task_when_a_dep_source_changes() {
    use std::io::Write;
    use std::time::{Duration, Instant};

    fn line_count(p: &std::path::Path) -> usize {
        std::fs::read_to_string(p)
            .map(|s| s.lines().count())
            .unwrap_or(0)
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.txt"), "v0").unwrap();
    // One giant.yaml: `targets:` for the engine, `tasks:` for the
    // porcelain. The task depends on target `lib`; `lib`'s input is
    // src/*.txt - which is NOT one of the task's own inputs.
    write_config(
        root,
        r#"
workspace: { name: w }
targets:
  - name: "lib"
    inputs: ["src/**/*.txt"]
    outputs: ["out/lib.txt"]
    command: "mkdir -p out && cp src/lib.txt out/lib.txt"
tasks:
  app:
    deps: ["//:lib"]
    command: "printf 'ran\n' >> runs.log"
"#,
    );

    let runs = root.join("runs.log");
    let mut child = Command::new(giant_task_bin())
        // giant-task's own flags precede the task name.
        .args(["--watch", "app"])
        .current_dir(root)
        // giant-task shells out to this binary for both `giant build`
        // (deps) and `giant session` (the watch channel).
        .env("GIANT_TASK_BUILD_BIN", giant_bin())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    // Wait for the initial run.
    let deadline = Instant::now() + Duration::from_secs(30);
    while line_count(&runs) < 1 {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("initial run never happened");
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Edit the DEP's source repeatedly until a rerun lands. Repeating is
    // robust against the subscription not being live for the first edit
    // (the session takes a moment to reach engine.ready).
    let deadline = Instant::now() + Duration::from_secs(40);
    let mut v = 1;
    let reran = loop {
        let mut f = std::fs::File::create(root.join("src/lib.txt")).unwrap();
        write!(f, "v{v}").unwrap();
        f.sync_all().unwrap();
        drop(f);
        v += 1;
        std::thread::sleep(Duration::from_millis(700));
        if line_count(&runs) >= 2 {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
    };

    // Stop the watcher (SIGINT → clean break → drains the session).
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let _ = child.wait();

    assert!(
        reran,
        "editing a dependency's source must re-run the watched task"
    );
}
