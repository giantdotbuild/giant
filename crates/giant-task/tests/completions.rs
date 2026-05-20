//! `giant-task --completions <shell>` smoke tests - same shape as the
//! core's completions tests but exercises giant-task's flag.

use std::process::Command;

fn giant_task_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant-task"))
}

fn run(shell: &str) -> String {
    let out = Command::new(giant_task_bin())
        .args(["--completions", shell])
        .output()
        .expect("spawn giant-task");
    assert!(
        out.status.success(),
        "--completions {shell} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8 completion script")
}

#[test]
fn bash_script_works() {
    let s = run("bash");
    assert!(s.len() > 200);
    assert!(s.contains("giant-task"));
}

#[test]
fn zsh_script_works() {
    let s = run("zsh");
    assert!(s.contains("#compdef"));
    assert!(s.contains("giant-task"));
}

#[test]
fn fish_script_works() {
    let s = run("fish");
    assert!(s.contains("complete -c giant-task"));
}

#[test]
fn powershell_script_works() {
    let s = run("powershell");
    assert!(s.contains("giant-task"));
}

#[test]
fn elvish_script_works() {
    let s = run("elvish");
    assert!(s.contains("giant-task"));
}

#[test]
fn nushell_script_works() {
    let s = run("nushell");
    assert!(s.contains("extern"));
    assert!(s.contains("giant-task"));
}
