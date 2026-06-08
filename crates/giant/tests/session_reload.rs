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

/// Read event lines until `pred` matches, returning that event (or None on EOF
/// / cap). Capped so a missing event fails instead of hanging.
fn read_until(out: &mut BufReader<ChildStdout>, pred: impl Fn(&Event) -> bool) -> Option<Event> {
    for _ in 0..500 {
        let mut line = String::new();
        if out.read_line(&mut line).unwrap_or(0) == 0 {
            return None;
        }
        let Ok(event) = serde_json::from_str::<Event>(line.trim()) else {
            continue;
        };
        if pred(&event) {
            return Some(event);
        }
    }
    None
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
  - name: "a"
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
        before.contains(&"//:a".to_string()),
        "initial catalog should list target a; got {before:?}"
    );
    assert!(
        !before.contains(&"//:b".to_string()),
        "target b shouldn't exist yet"
    );

    // Add target b, then force a reload.
    write_config(
        ws,
        r#"
workspace: { name: reload }
targets:
  - name: "a"
    command: "true"
    cache: false
  - name: "b"
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
        after.contains(&"//:b".to_string()),
        "reload must surface the newly-added target b; got {after:?}"
    );

    shutdown(child, stdin);
}

#[test]
fn query_status_reports_cache_state_and_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    write_config(
        ws,
        r#"
workspace: { name: q }
targets:
  - name: "a"
    command: "true"
    outputs: ["o"]
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

    // engine.hello advertises protocol 2 + the query.status capability.
    let hello = read_until(&mut out, |e| matches!(e, Event::EngineHello { .. })).expect("hello");
    if let Event::EngineHello {
        protocol,
        capabilities,
        ..
    } = hello
    {
        assert_eq!(protocol, 2);
        assert!(
            capabilities.iter().any(|c| c == "query.status"),
            "got {capabilities:?}"
        );
    }
    read_catalog_until(&mut out, |e| matches!(e, Event::EngineReady));

    // Never built → stale, but the key is computable.
    writeln!(
        stdin,
        r#"{{"c":"query.status","command_id":"q1","targets":["//:a"]}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let reply = read_until(&mut out, |e| {
        matches!(e, Event::QueryStatus { command_id, .. } if command_id.as_deref() == Some("q1"))
    })
    .expect("query.status reply");
    if let Event::QueryStatus { targets, .. } = reply {
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id.as_str(), "//:a");
        assert_eq!(targets[0].state, "stale", "never-built target is stale");
        assert!(!targets[0].key.is_empty(), "stale target still has a key");
    }

    shutdown(child, stdin);
}

#[test]
fn logs_get_replays_captured_output() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    write_config(
        ws,
        r#"
workspace: { name: q }
targets:
  - name: "a"
    command: "echo hello-logs; touch marker"
    outputs: ["marker"]
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

    read_catalog_until(&mut out, |e| matches!(e, Event::EngineReady));

    // Build a, which echoes to stdout; the engine captures it into the cache
    // (the target is cacheable, so an AC entry with the stdout blob is written).
    writeln!(
        stdin,
        r#"{{"c":"build","command_id":"b1","targets":["//:a"]}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let finished = read_until(&mut out, |e| matches!(e, Event::BuildFinished { .. }));
    assert!(finished.is_some(), "build should finish");

    // Replay the captured logs.
    writeln!(
        stdin,
        r#"{{"c":"logs.get","command_id":"L1","target":"//:a"}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut lines = Vec::new();
    for _ in 0..500 {
        let mut line = String::new();
        if out.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let Ok(event) = serde_json::from_str::<Event>(line.trim()) else {
            continue;
        };
        match event {
            Event::LogsLine {
                command_id, line, ..
            } if command_id.as_deref() == Some("L1") => lines.push(line),
            Event::LogsEnd { command_id, .. } if command_id.as_deref() == Some("L1") => break,
            _ => {}
        }
    }
    assert!(
        lines.iter().any(|l| l.contains("hello-logs")),
        "replay should include the echoed line; got {lines:?}"
    );

    shutdown(child, stdin);
}

#[test]
fn query_explain_returns_breakdown() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(ws.join("in.txt"), "hi\n").unwrap();
    write_config(
        ws,
        r#"
workspace: { name: e }
targets:
  - name: "a"
    command: "cat in.txt"
    inputs: ["in.txt"]
    outputs: ["o"]
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

    read_catalog_until(&mut out, |e| matches!(e, Event::EngineReady));

    writeln!(
        stdin,
        r#"{{"c":"query.explain","command_id":"e1","target":"//:a"}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let reply = read_until(&mut out, |e| {
        matches!(e, Event::QueryExplained { command_id, .. } if command_id.as_deref() == Some("e1"))
    })
    .expect("query.explained reply");
    if let Event::QueryExplained {
        command,
        cached,
        key,
        file_inputs,
        ..
    } = reply
    {
        assert_eq!(command, "cat in.txt");
        assert!(!cached, "never built → not cached");
        assert!(!key.is_empty());
        assert!(
            file_inputs.iter().any(|f| f.path == "in.txt"),
            "in.txt should feed the key; got {file_inputs:?}"
        );
    }

    shutdown(child, stdin);
}

#[test]
fn query_explain_reports_cache_hit_after_build() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    write_config(
        ws,
        r#"
workspace: { name: e }
targets:
  - name: "a"
    command: "touch built"
    outputs: ["built"]
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

    read_catalog_until(&mut out, |e| matches!(e, Event::EngineReady));

    writeln!(
        stdin,
        r#"{{"c":"build","command_id":"b1","targets":["//:a"]}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    read_until(&mut out, |e| matches!(e, Event::BuildFinished { .. })).expect("build finishes");

    writeln!(
        stdin,
        r#"{{"c":"query.explain","command_id":"e2","target":"//:a"}}"#
    )
    .unwrap();
    stdin.flush().unwrap();

    let reply = read_until(&mut out, |e| {
        matches!(e, Event::QueryExplained { command_id, .. } if command_id.as_deref() == Some("e2"))
    })
    .expect("query.explained reply");
    if let Event::QueryExplained {
        cached, cache_hit, ..
    } = reply
    {
        assert!(cached, "built → cached");
        let hit = cache_hit.expect("cache_hit present on a hit");
        assert_eq!(hit.exit_code, 0);
        assert!(
            hit.outputs.iter().any(|o| o.path == "built"),
            "outputs should list the produced file; got {:?}",
            hit.outputs
        );
        assert!(!hit.outputs_content_hash.is_empty());
    }

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
  - name: "a"
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
    assert!(before.contains(&"//:a".to_string()), "got {before:?}");

    // Edit giant.yaml - no command. The always-on watcher should notice
    // and trigger a reload on its own.
    write_config(
        ws,
        r#"
workspace: { name: reload }
targets:
  - name: "a"
    command: "true"
    cache: false
  - name: "c"
    command: "true"
    cache: false
"#,
    );

    let after = read_catalog_until(&mut out, |e| matches!(e, Event::CatalogReady));
    assert!(
        after.contains(&"//:c".to_string()),
        "the watcher should auto-reload and surface target c; got {after:?}"
    );

    shutdown(child, stdin);
}
