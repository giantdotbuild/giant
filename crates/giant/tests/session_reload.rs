//! `giant session` live config reload (TDD-0014). Drives the engine over
//! stdio: read the initial catalog, edit `giant.yaml`, force a reload,
//! and assert the re-emitted catalog reflects the change.

use giant::events::Event;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, Stdio};

fn giant_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant"))
}

/// Read NDJSON event lines until `stop` returns true on a parsed event,
/// collecting every `target.described` id seen along the way. Capped so a
/// missing event fails the test instead of hanging forever.
fn read_catalog_until(
    out: &mut BufReader<ChildStdout>,
    stop: impl Fn(&Event) -> bool,
) -> Vec<String> {
    let mut ids = Vec::new();
    for _ in 0..500 {
        let mut line = String::new();
        if out.read_line(&mut line).unwrap_or(0) == 0 {
            break; // EOF
        }
        let Ok(event) = serde_json::from_str::<Event>(line.trim()) else {
            continue;
        };
        if let Event::TargetDescribed { id, .. } = &event {
            ids.push(id.as_str().to_string());
        }
        if stop(&event) {
            break;
        }
    }
    ids
}

fn write_config(ws: &std::path::Path, body: &str) {
    std::fs::write(ws.join("giant.yaml"), body).unwrap();
}

fn shutdown(mut child: Child, stdin: std::process::ChildStdin) {
    drop(stdin); // EOF → session drains + exits
    let _ = child.wait();
}

#[test]
fn config_reload_re_emits_catalog_with_new_target() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    write_config(
        ws,
        r#"
workspace: { name: reload }
targets:
  - id: "a"
    command: "true"
    cache: false
"#,
    );

    let mut child = Command::new(giant_bin())
        .args(["session", "--events", "ndjson"])
        .current_dir(ws)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn giant session");
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());

    // Initial catalog, terminated by engine.ready.
    let before = read_catalog_until(&mut out, |e| matches!(e, Event::EngineReady));
    assert!(
        before.contains(&"a".to_string()),
        "initial catalog should list target a; got {before:?}"
    );
    assert!(
        !before.contains(&"b".to_string()),
        "target b shouldn't exist yet"
    );

    // Add target b, then force a reload.
    write_config(
        ws,
        r#"
workspace: { name: reload }
targets:
  - id: "a"
    command: "true"
    cache: false
  - id: "b"
    command: "true"
    cache: false
"#,
    );
    writeln!(stdin, r#"{{"c":"config.reload","command_id":"r1"}}"#).unwrap();
    stdin.flush().unwrap();

    // The re-emitted catalog (after catalog.invalidating) ends at
    // catalog.ready and must now include b.
    let after = read_catalog_until(&mut out, |e| matches!(e, Event::CatalogReady));
    assert!(
        after.contains(&"b".to_string()),
        "reload must surface the newly-added target b; got {after:?}"
    );

    shutdown(child, stdin);
}

#[test]
fn editing_giant_yaml_auto_reloads_via_the_watcher() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    write_config(
        ws,
        r#"
workspace: { name: reload }
targets:
  - id: "a"
    command: "true"
    cache: false
"#,
    );

    let mut child = Command::new(giant_bin())
        .args(["session", "--events", "ndjson"])
        .current_dir(ws)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn giant session");
    let stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());

    let before = read_catalog_until(&mut out, |e| matches!(e, Event::EngineReady));
    assert!(before.contains(&"a".to_string()), "got {before:?}");

    // Edit giant.yaml - no command. The always-on watcher should notice
    // and trigger a reload on its own.
    write_config(
        ws,
        r#"
workspace: { name: reload }
targets:
  - id: "a"
    command: "true"
    cache: false
  - id: "c"
    command: "true"
    cache: false
"#,
    );

    let after = read_catalog_until(&mut out, |e| matches!(e, Event::CatalogReady));
    assert!(
        after.contains(&"c".to_string()),
        "the watcher should auto-reload and surface target c; got {after:?}"
    );

    shutdown(child, stdin);
}
