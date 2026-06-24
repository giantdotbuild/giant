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

/// Fails every AC PUT with a fixed status, accepts CAS. Used to check that
/// AC-write failures surface and don't fail the build.
#[derive(Clone)]
struct AcFailing {
    cas: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    ac_status: u16,
}

impl AcFailing {
    fn new(ac_status: u16) -> Self {
        Self {
            cas: Arc::new(Mutex::new(HashMap::new())),
            ac_status,
        }
    }
}

impl Respond for AcFailing {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let path = req.url.path().to_string();
        match req.method.as_ref() {
            "PUT" if path.starts_with("/ac/") => ResponseTemplate::new(self.ac_status),
            "PUT" => {
                self.cas.lock().unwrap().insert(path, req.body.clone());
                ResponseTemplate::new(200)
            }
            "GET" | "HEAD" => {
                if self.cas.lock().unwrap().contains_key(&path) {
                    ResponseTemplate::new(200)
                } else {
                    ResponseTemplate::new(404)
                }
            }
            _ => ResponseTemplate::new(405),
        }
    }
}

/// Start a mock server whose AC PUTs all fail with `ac_status`.
async fn ac_failing_server(ac_status: u16) -> MockServer {
    let server = MockServer::start().await;
    let store = AcFailing::new(ac_status);
    for m in ["PUT", "GET", "HEAD"] {
        Mock::given(method(m))
            .respond_with(store.clone())
            .mount(&server)
            .await;
    }
    server
}

/// Run a one-target build against `server_uri` and return its stderr. The
/// build must succeed - a failing remote never fails the build.
async fn build_with_remote(server_uri: &str) -> String {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(ws.join("in.txt"), "hello\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        format!(
            r#"
workspace:
  name: ac_failing
cache:
  dir: ./cache
remote:
  enabled: true
  url: "{server_uri}"
  skip_head: true
  auth:
    kind: none
targets:
  - name: "demo"
    inputs: ["in.txt"]
    outputs: ["out.txt"]
    command: "cp in.txt out.txt"
"#
        ),
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "a failing AC write must not fail the build: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_validation_rejection_is_surfaced_with_the_fix() {
    let server = ac_failing_server(400).await;
    let stderr = build_with_remote(&server.uri()).await;
    assert!(
        stderr.contains("--disable_http_ac_validation"),
        "a 400 AC rejection should name the bazel-remote fix; got: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn other_ac_write_failures_are_surfaced() {
    let server = ac_failing_server(503).await;
    let stderr = build_with_remote(&server.uri()).await;
    assert!(
        stderr.contains("remote action-cache write failed"),
        "a non-400 AC failure should still surface; got: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_creds_disable_remote_without_failing_the_build() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(ws.join("in.txt"), "hello\n").unwrap();
    // Basic auth pointing at env vars that are unset in the build process.
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: missing_creds
cache:
  dir: ./cache
remote:
  enabled: true
  url: "https://cache.invalid"
  auth:
    kind: basic
    username_env: GIANT_TEST_MISSING_USER
    password_env: GIANT_TEST_MISSING_PASS
targets:
  - name: "demo"
    inputs: ["in.txt"]
    outputs: ["out.txt"]
    command: "cp in.txt out.txt"
"#,
    )
    .unwrap();

    let out = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .env_remove("GIANT_TEST_MISSING_USER")
        .env_remove("GIANT_TEST_MISSING_PASS")
        .output()
        .expect("spawn");
    assert!(
        out.status.success(),
        "missing remote creds must not fail the build: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("remote cache disabled"),
        "expected a soft-disable warning; got: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("out.txt")).unwrap().trim(),
        "hello"
    );
}

// ============================================================================
// GitHub Actions cache backend
// ============================================================================

/// In-memory GitHub Actions cache service: the three Twirp endpoints plus
/// the signed-URL blob store they hand out. Twirp calls must carry the
/// runtime token; the signed URLs must not. The first Twirp call is
/// answered 429 to exercise the client's retry.
#[derive(Clone, Default)]
struct GhaService {
    base: Arc<Mutex<String>>,
    blobs: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    rate_limited_once: Arc<std::sync::atomic::AtomicBool>,
}

const TWIRP_BASE: &str = "/twirp/github.actions.results.api.v1.CacheService/";

impl Respond for GhaService {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let path = req.url.path().to_string();
        let base = self.base.lock().unwrap().clone();

        if let Some(method_name) = path.strip_prefix(TWIRP_BASE) {
            let authed = req
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v == "Bearer t-token");
            if !authed {
                return ResponseTemplate::new(401);
            }
            if !self
                .rate_limited_once
                .swap(true, std::sync::atomic::Ordering::Relaxed)
            {
                return ResponseTemplate::new(429).insert_header("retry-after", "1");
            }
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let key = body["key"].as_str().unwrap_or_default().to_string();
            assert!(
                !body["version"].as_str().unwrap_or_default().is_empty(),
                "twirp requests must carry a version"
            );
            return match method_name {
                "GetCacheEntryDownloadURL" => {
                    let hit = self.blobs.lock().unwrap().contains_key(&key);
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "ok": hit,
                        "signed_download_url": if hit { format!("{base}/blob/{key}") } else { String::new() },
                    }))
                }
                "CreateCacheEntry" => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "ok": true,
                    "signed_upload_url": format!("{base}/upload/{key}"),
                })),
                "FinalizeCacheEntryUpload" => ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"ok": true, "entry_id": "1"})),
                _ => ResponseTemplate::new(404),
            };
        }

        if let Some(key) = path.strip_prefix("/upload/") {
            if req.headers.get("x-ms-blob-type").is_none() {
                return ResponseTemplate::new(400);
            }
            self.blobs
                .lock()
                .unwrap()
                .insert(key.to_string(), req.body.clone());
            return ResponseTemplate::new(201);
        }
        if let Some(key) = path.strip_prefix("/blob/") {
            return match self.blobs.lock().unwrap().get(key) {
                Some(bytes) => ResponseTemplate::new(200).set_body_bytes(bytes.clone()),
                None => ResponseTemplate::new(404),
            };
        }
        ResponseTemplate::new(404)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gha_cache_round_trip() {
    let server = MockServer::start().await;
    let svc = GhaService::default();
    *svc.base.lock().unwrap() = server.uri();
    for m in ["POST", "PUT", "GET"] {
        Mock::given(method(m))
            .respond_with(svc.clone())
            .mount(&server)
            .await;
    }

    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path();
    std::fs::write(ws.join("in.txt"), "hello gha\n").unwrap();
    std::fs::write(
        ws.join("giant.yaml"),
        r#"
workspace:
  name: gha_round_trip
cache:
  dir: ./cache
remote:
  enabled: true
  kind: github_actions
targets:
  - name: "demo"
    inputs: ["in.txt"]
    outputs: ["out.txt"]
    command: "cp in.txt out.txt"
"#,
    )
    .unwrap();

    let build = |ws: &std::path::Path| {
        Command::new(giant_bin())
            .arg("build")
            .current_dir(ws)
            // The backend is gated on running inside Actions; fake it.
            .env("GITHUB_ACTIONS", "true")
            .env("ACTIONS_RESULTS_URL", server.uri())
            .env("ACTIONS_RUNTIME_TOKEN", "t-token")
            .output()
            .expect("spawn")
    };

    // Outside Actions (no GITHUB_ACTIONS), the same config is a quiet
    // no-op: the build succeeds and nothing reaches the mock service.
    let out0 = Command::new(giant_bin())
        .arg("build")
        .current_dir(ws)
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("spawn");
    assert!(out0.status.success(), "local-mode build failed");
    assert!(
        svc.blobs.lock().unwrap().is_empty(),
        "no uploads expected outside Actions"
    );
    let _ = Command::new(giant_bin())
        .args(["clean", "-y"])
        .current_dir(ws)
        .output()
        .unwrap();
    std::fs::remove_file(ws.join("out.txt")).ok();

    let out = build(ws);
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let keys: Vec<String> = svc.blobs.lock().unwrap().keys().cloned().collect();
    assert!(
        keys.iter().any(|k| k.starts_with("giant-ac-")),
        "expected an AC entry; got {keys:?}"
    );
    assert!(
        keys.iter().any(|k| k.starts_with("giant-cas-")),
        "expected a CAS blob; got {keys:?}"
    );

    let _ = Command::new(giant_bin())
        .args(["clean", "-y"])
        .current_dir(ws)
        .output()
        .unwrap();
    std::fs::remove_file(ws.join("out.txt")).ok();

    let out2 = build(ws);
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
        "hello gha"
    );
}
