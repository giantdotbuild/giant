//! End-to-end remote-cache test. Uses `wiremock` as an in-process HTTP
//! server, configures Giant to point at it, and verifies:
//!   1. After a build, AC + CAS PUT requests appear on the server.
//!   2. After `giant clean` + a rebuild, the server's stored blobs
//!      restore the workspace (no local rebuild needed beyond the
//!      restore).

#![cfg(feature = "remote")]

use std::collections::HashMap;
use std::process::Command;
use std::sync::{Arc, Mutex};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

fn giant_bin() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_giant"))
}

/// Shared in-memory blob store used by the test server. Maps full URL
/// path (e.g. `/cas/abc...`) → body bytes.
#[derive(Clone, Default)]
struct Store(Arc<Mutex<HashMap<String, Vec<u8>>>>);

impl Respond for Store {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let path = req.url.path().to_string();
        let m = req.method.as_ref();
        let mut store = self.0.lock().unwrap();
        match m {
            "PUT" => {
                store.insert(path, req.body.clone());
                ResponseTemplate::new(200)
            }
            "GET" => match store.get(&path) {
                Some(bytes) => ResponseTemplate::new(200).set_body_bytes(bytes.clone()),
                None => ResponseTemplate::new(404),
            },
            "HEAD" => {
                if store.contains_key(&path) {
                    ResponseTemplate::new(200)
                } else {
                    ResponseTemplate::new(404)
                }
            }
            _ => ResponseTemplate::new(405),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_cache_round_trip() {
    let server = MockServer::start().await;
    let store = Store::default();
    // One catch-all responder for /ac/* and /cas/* requests.
    Mock::given(method("PUT"))
        .respond_with(store.clone())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(store.clone())
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .respond_with(store.clone())
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(ws.join("in.txt"), "hello remote\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        format!(
            r#"
workspace:
  name: remote_round_trip
cache:
  dir: ./cache
remote:
  enabled: true
  url: "{}"
  skip_head: true
  auth:
    kind: none
targets:
  - name: "demo"
    inputs: ["in.txt"]
    outputs: ["out.txt"]
    command: "cp in.txt out.txt"
"#,
            server.uri()
        ),
    )
    .unwrap();

    // First build: populates local + uploads to the (mock) remote.
    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The uploader is async; give it a beat to flush. Increased to be
    // CI-friendly; locally the uploads usually drain in <10ms.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Inspect the server's store: at least one /ac/ and one /cas/
    // entry should be present.
    let keys: Vec<String> = store.0.lock().unwrap().keys().cloned().collect();
    let n_ac = keys.iter().filter(|k| k.starts_with("/ac/")).count();
    let n_cas = keys.iter().filter(|k| k.starts_with("/cas/")).count();
    assert!(
        n_ac >= 1,
        "expected at least 1 AC entry on remote; got: {keys:?}"
    );
    assert!(
        n_cas >= 1,
        "expected at least 1 CAS blob on remote; got: {keys:?}"
    );

    // Wipe local cache + the workspace output. Next build must restore
    // from the remote.
    let _ = Command::new(giant_bin())
        .args(["clean", "-y"])
        .current_dir(ws)
        .output()
        .unwrap();
    std::fs::remove_file(ws.join("out.txt")).ok();

    let out2 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn");
    assert!(
        out2.status.success(),
        "second build failed: {}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let stdout = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout.contains("REMOTE"),
        "expected a REMOTE cache hit; got stdout: {stdout}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap().trim(),
        "hello remote"
    );
}
