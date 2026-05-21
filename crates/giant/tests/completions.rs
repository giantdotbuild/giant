//! `giant completions <shell>` smoke tests - for each supported shell
//! we just verify the script is non-empty and mentions the binary
//! name. Detailed scripting correctness is clap_complete's job; we
//! check the wiring.

use std::process::Command;

fn giant_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant"))
}

fn run(shell: &str) -> String {
    let out = Command::new(giant_bin())
        .args(["completions", shell])
        .output()
        .expect("spawn giant");
    assert!(
        out.status.success(),
        "completions {shell} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8 completion script")
}

#[test]
fn bash_script_is_non_empty_and_mentions_giant() {
    let s = run("bash");
    assert!(s.len() > 200, "suspiciously short: {} bytes", s.len());
    assert!(s.contains("giant"));
}

#[test]
fn zsh_script_is_non_empty_and_mentions_giant() {
    let s = run("zsh");
    assert!(s.len() > 200);
    assert!(s.contains("giant"));
    assert!(s.contains("#compdef"));
}

#[test]
fn fish_script_is_non_empty_and_mentions_giant() {
    let s = run("fish");
    assert!(s.len() > 200);
    assert!(s.contains("giant"));
    // fish completion uses `complete -c <name>`.
    assert!(s.contains("complete -c giant"));
}

#[test]
fn powershell_script_is_non_empty_and_mentions_giant() {
    let s = run("powershell");
    assert!(s.len() > 200);
    assert!(s.contains("giant"));
}

#[test]
fn elvish_script_is_non_empty_and_mentions_giant() {
    let s = run("elvish");
    assert!(s.len() > 200);
    assert!(s.contains("giant"));
}

#[test]
fn nushell_script_is_non_empty_and_mentions_giant() {
    let s = run("nushell");
    assert!(s.len() > 200);
    assert!(s.contains("giant"));
    // nushell uses `extern "<cmd> <sub>" [ ... ]` for completion defs.
    assert!(s.contains("extern"));
}
