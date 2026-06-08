//! `giant-clean` end to end: populate a fake cache, then clean it.

use std::process::Command;

fn bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant-clean"))
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("giant.yaml"),
        "workspace: { name: w }\ncache: { dir: ./cache }\n",
    )
    .unwrap();
    dir
}

/// Drop a fake AC entry + version marker so the cache is non-empty.
fn seed_cache(ws: &std::path::Path) {
    let ac = ws.join("cache/ac/ab");
    std::fs::create_dir_all(&ac).unwrap();
    std::fs::write(ac.join("abcd"), r#"{"target_id":"//:a"}"#).unwrap();
    std::fs::write(ws.join("cache/version"), "1\n").unwrap();
}

#[test]
fn full_wipe_with_yes_clears_cache() {
    let dir = workspace();
    let ws = dir.path();
    seed_cache(ws);

    let out = Command::new(bin())
        .arg("-y")
        .current_dir(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!ws.join("cache/version").exists(), "cache should be wiped");
    assert!(!ws.join("cache/ac/ab/abcd").exists());
}

#[test]
fn empty_cache_is_a_noop() {
    let dir = workspace();
    let out = Command::new(bin())
        .arg("-y")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(out.status.success());
}

#[test]
fn requires_yes_when_non_interactive() {
    let dir = workspace();
    seed_cache(dir.path());
    // stdin is not a tty under `output()`, so the confirmation can't be
    // answered - without -y it must bail rather than hang.
    let out = Command::new(bin())
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!out.status.success(), "should refuse without -y");
    assert!(dir.path().join("cache/version").exists(), "nothing deleted");
}
