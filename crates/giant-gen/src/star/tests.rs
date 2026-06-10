use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use super::{StdPin, StdSource};

/// Write `giant.star` into `dir` and return its path.
fn script(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("giant.star");
    fs::write(&path, body).unwrap();
    path
}

/// `super::generate` with no std source - the common case here.
fn generate(script: &Path, root: &Path) -> anyhow::Result<Vec<super::Emitted>> {
    super::generate(script, root, &StdSource::new(None, None))
}

fn run(script: &Path, infix: &str, out_root: &Path, root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    super::run(script, infix, out_root, root, &StdSource::new(None, None))
}

/// An on-disk std collection holding one `mod.star` with a `name()` helper.
fn fake_collection() -> TempDir {
    let d = TempDir::new().unwrap();
    fs::write(
        d.path().join("mod.star"),
        "def name():\n    return \"fromstd\"\n",
    )
    .unwrap();
    d
}

const USES_STD: &str = "load(\"@std//mod.star\", \"name\")\ndef generate(ws):\n    target(name = name(), command = \"true\", outputs = [\"o\"])\n";

#[test]
fn target_builds_wire_form() {
    let tmp = TempDir::new().unwrap();
    let s = script(
        tmp.path(),
        r#"
def generate(ws):
    return [target(name = "build", command = "go build", outputs = ["bin/x"], deps = ["//:dep"])]
"#,
    );
    let out = generate(&s, tmp.path()).unwrap();
    assert_eq!(out.len(), 1);
    let t = &out[0];
    assert_eq!(t.package, "");
    assert_eq!(t.wire.name, "build");
    assert_eq!(t.wire.command, "go build");
    assert_eq!(t.wire.outputs, vec!["bin/x".to_string()]);
    assert_eq!(t.wire.deps, vec!["//:dep".to_string()]);
    assert!(t.wire.remote_cache, "defaults to true");
}

#[test]
fn missing_generate_is_an_error() {
    let tmp = TempDir::new().unwrap();
    let s = script(tmp.path(), "x = 1\n");
    let err = generate(&s, tmp.path()).unwrap_err();
    assert!(err.to_string().contains("generate"), "{err}");
}

#[test]
fn package_precedence_explicit_then_cwd_then_root() {
    let tmp = TempDir::new().unwrap();
    let s = script(
        tmp.path(),
        r#"
def generate(ws):
    return [
        target(name = "a", command = "true", outputs = ["o"], package = "svc/api"),
        target(name = "b", command = "true", outputs = ["o"], cwd = "cmd/tool"),
        target(name = "c", command = "true", outputs = ["o"]),
    ]
"#,
    );
    let out = generate(&s, tmp.path()).unwrap();
    let pkg = |name: &str| {
        out.iter()
            .find(|e| e.wire.name == name)
            .map(|e| e.package.as_str())
            .unwrap()
    };
    assert_eq!(pkg("a"), "svc/api"); // explicit package wins
    assert_eq!(pkg("b"), "cmd/tool"); // derived from cwd
    assert_eq!(pkg("c"), ""); // root
}

#[test]
fn targets_built_in_helpers_are_collected() {
    // target() registers when called (the Bazel/Buck2 model), so targets built
    // inside helper functions are collected regardless of what generate returns.
    let tmp = TempDir::new().unwrap();
    let s = script(
        tmp.path(),
        r#"
def lib(ws):
    return [target(name = "x", command = "true", outputs = ["o"])]

def generate(ws):
    lib(ws)
    target(name = "y", command = "true", outputs = ["o"])
"#,
    );
    let out = generate(&s, tmp.path()).unwrap();
    let mut names: Vec<_> = out.iter().map(|e| e.wire.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["x".to_string(), "y".to_string()]);
}

#[test]
fn emit_groups_by_package_and_is_deterministic() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let s = script(
        root,
        r#"
def generate(ws):
    return [
        target(name = "api", command = "true", outputs = ["o"], package = "svc/api"),
        target(name = "root", command = "true", outputs = ["o"]),
    ]
"#,
    );
    let written = run(&s, "gen", root, root).unwrap();
    // Root package and svc/api each get a file.
    assert!(root.join("giant.gen.yaml").exists());
    assert!(root.join("svc/api/giant.gen.yaml").exists());
    assert_eq!(written.len(), 2);

    let first = fs::read_to_string(root.join("svc/api/giant.gen.yaml")).unwrap();
    run(&s, "gen", root, root).unwrap();
    let second = fs::read_to_string(root.join("svc/api/giant.gen.yaml")).unwrap();
    assert_eq!(first, second, "emit must be byte-identical across runs");
    assert!(first.contains("name: api"));
}

#[test]
fn prune_removes_files_no_longer_generated() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let two = script(
        root,
        r#"
def generate(ws):
    return [
        target(name = "a", command = "true", outputs = ["o"], package = "svc/a"),
        target(name = "b", command = "true", outputs = ["o"], package = "svc/b"),
    ]
"#,
    );
    run(&two, "gen", root, root).unwrap();
    assert!(root.join("svc/b/giant.gen.yaml").exists());

    // Regenerate with only svc/a; svc/b's file must be pruned.
    fs::write(
        &two,
        "def generate(ws):\n    return [target(name = \"a\", command = \"true\", outputs = [\"o\"], package = \"svc/a\")]\n",
    )
    .unwrap();
    run(&two, "gen", root, root).unwrap();
    assert!(root.join("svc/a/giant.gen.yaml").exists());
    assert!(
        !root.join("svc/b/giant.gen.yaml").exists(),
        "stale file pruned"
    );
}

#[test]
fn ws_glob_is_sorted() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(root.join("pkg/b.go"), "").unwrap();
    fs::write(root.join("pkg/a.go"), "").unwrap();
    fs::write(root.join("pkg/c.txt"), "").unwrap();
    let s = script(
        root,
        r#"
def generate(ws):
    found = ws.glob("pkg/*.go")
    return [target(name = "n", command = "true", outputs = ["o"], tags = found)]
"#,
    );
    let out = generate(&s, root).unwrap();
    let tags: Vec<_> = out[0].wire.tags.iter().cloned().collect();
    assert_eq!(tags, vec!["pkg/a.go".to_string(), "pkg/b.go".to_string()]);
}

#[test]
fn loads_std_module_from_collection_dir() {
    let coll = fake_collection();
    let tmp = TempDir::new().unwrap();
    let s = script(tmp.path(), USES_STD);
    let std = StdSource::new(Some(coll.path().to_path_buf()), None);
    let out = super::generate(&s, tmp.path(), &std).unwrap();
    assert_eq!(out[0].wire.name, "fromstd");
}

#[test]
fn legacy_giant_alias_still_resolves() {
    // `@giant//` stays a deprecated alias for `@std//`.
    let coll = fake_collection();
    let tmp = TempDir::new().unwrap();
    let s = script(
        tmp.path(),
        "load(\"@giant//mod.star\", \"name\")\ndef generate(ws):\n    target(name = name(), command = \"true\", outputs = [\"o\"])\n",
    );
    let std = StdSource::new(Some(coll.path().to_path_buf()), None);
    let out = super::generate(&s, tmp.path(), &std).unwrap();
    assert_eq!(out[0].wire.name, "fromstd");
}

#[test]
fn std_load_without_a_source_is_a_clear_error() {
    let tmp = TempDir::new().unwrap();
    let s = script(tmp.path(), USES_STD);
    let e = generate(&s, tmp.path()).unwrap_err();
    assert!(e.to_string().contains("no std module source"), "{e}");
}

#[test]
fn path_like_std_module_names_are_rejected() {
    let coll = fake_collection();
    let tmp = TempDir::new().unwrap();
    let s = script(tmp.path(), "load(\"@std//../escape.star\", \"x\")\n");
    let std = StdSource::new(Some(coll.path().to_path_buf()), None);
    let e = super::generate(&s, tmp.path(), &std).unwrap_err();
    assert!(e.to_string().contains("invalid std module name"), "{e}");
}

/// A pin against a wiremock server standing in for raw.githubusercontent.com.
fn mock_pin(base: String, cache: &Path) -> StdPin {
    StdPin {
        repo: "giantdotbuild/giant-std".into(),
        rev: "v1".into(),
        base,
        cache: cache.to_path_buf(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pinned_module_fetches_once_then_reads_the_cache() {
    use wiremock::matchers::{method, path};
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("GET"))
        .and(path("/giantdotbuild/giant-std/v1/mod.star"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .set_body_string("def name():\n    return \"pinned\"\n"),
        )
        .expect(1) // the second generate must come from the disk cache
        .mount(&server)
        .await;

    let cache = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let s = script(tmp.path(), USES_STD);
    let std = StdSource::new(None, Some(mock_pin(server.uri(), cache.path())));

    for _ in 0..2 {
        let (s, root, std) = (s.clone(), tmp.path().to_path_buf(), std.clone());
        let out = tokio::task::spawn_blocking(move || super::generate(&s, &root, &std))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(out[0].wire.name, "pinned");
    }
    assert!(
        cache
            .path()
            .join("giantdotbuild/giant-std/v1/mod.star")
            .is_file()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_pinned_module_is_a_clear_error() {
    let server = wiremock::MockServer::start().await;
    // No mounted route: every GET is a 404.
    let cache = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let s = script(tmp.path(), "load(\"@std//nope.star\", \"x\")\n");
    let std = StdSource::new(None, Some(mock_pin(server.uri(), cache.path())));

    let root = tmp.path().to_path_buf();
    let e = tokio::task::spawn_blocking(move || super::generate(&s, &root, &std))
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        e.to_string().contains("no std module named 'nope.star'"),
        "{e}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn on_disk_collection_wins_over_the_pin() {
    let server = wiremock::MockServer::start().await; // would 404 if asked
    let coll = fake_collection();
    let cache = TempDir::new().unwrap();
    let tmp = TempDir::new().unwrap();
    let s = script(tmp.path(), USES_STD);
    let std = StdSource::new(
        Some(coll.path().to_path_buf()),
        Some(mock_pin(server.uri(), cache.path())),
    );

    let root = tmp.path().to_path_buf();
    let out = tokio::task::spawn_blocking(move || super::generate(&s, &root, &std))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(out[0].wire.name, "fromstd");
}

#[test]
fn loads_repo_local_star_file() {
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("lib.star"),
        "def mk(): return \"fromlib\"\n",
    )
    .unwrap();
    let s = script(
        tmp.path(),
        r#"
load("lib.star", "mk")

def generate(ws):
    target(name = mk(), command = "true", outputs = ["o"])
"#,
    );
    let out = generate(&s, tmp.path()).unwrap();
    assert_eq!(out[0].wire.name, "fromlib");
}

#[test]
fn ws_exec_honors_cwd() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/marker"), "in-src\n").unwrap();
    let s = script(
        root,
        r#"
def generate(ws):
    out = ws.exec(["cat", "marker"], cwd = "src")
    target(name = "n", command = out.stdout, outputs = ["o"])
"#,
    );
    let out = generate(&s, root).unwrap();
    assert_eq!(out[0].wire.command, "in-src\n");
}

#[test]
fn ws_read_returns_contents() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    fs::write(root.join("go.mod"), "module example.com/x\n").unwrap();
    let s = script(
        root,
        r#"
def generate(ws):
    content = ws.read("go.mod")
    return [target(name = "n", command = content, outputs = ["o"])]
"#,
    );
    let out = generate(&s, root).unwrap();
    assert_eq!(out[0].wire.command, "module example.com/x\n");
}
