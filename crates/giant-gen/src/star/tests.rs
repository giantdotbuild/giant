use std::fs;
use std::path::Path;

use tempfile::TempDir;

/// Write `giant.star` into `dir` and return its path.
fn script(dir: &Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("giant.star");
    fs::write(&path, body).unwrap();
    path
}

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
    let out = super::generate(&s, tmp.path()).unwrap();
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
    let err = super::generate(&s, tmp.path()).unwrap_err();
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
    let out = super::generate(&s, tmp.path()).unwrap();
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
    let out = super::generate(&s, tmp.path()).unwrap();
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
    let written = super::run(&s, "gen", root, root).unwrap();
    // Root package and svc/api each get a file.
    assert!(root.join("giant.gen.yaml").exists());
    assert!(root.join("svc/api/giant.gen.yaml").exists());
    assert_eq!(written.len(), 2);

    let first = fs::read_to_string(root.join("svc/api/giant.gen.yaml")).unwrap();
    super::run(&s, "gen", root, root).unwrap();
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
    super::run(&two, "gen", root, root).unwrap();
    assert!(root.join("svc/b/giant.gen.yaml").exists());

    // Regenerate with only svc/a; svc/b's file must be pruned.
    fs::write(
        &two,
        "def generate(ws):\n    return [target(name = \"a\", command = \"true\", outputs = [\"o\"], package = \"svc/a\")]\n",
    )
    .unwrap();
    super::run(&two, "gen", root, root).unwrap();
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
    let out = super::generate(&s, root).unwrap();
    let tags: Vec<_> = out[0].wire.tags.iter().cloned().collect();
    assert_eq!(tags, vec!["pkg/a.go".to_string(), "pkg/b.go".to_string()]);
}

/// The in-repo std collection (`std/`), for `@std//` resolution in tests
/// without touching the process-global `GIANT_STD`.
fn std_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../std")
}

#[test]
fn loads_go_stdlib_from_collection() {
    // load("@std//go.star", ...) resolves to the shipped std collection;
    // bin_name is a pure helper, so this needs no go toolchain.
    let tmp = TempDir::new().unwrap();
    let s = script(
        tmp.path(),
        r#"
load("@std//go.star", "bin_name")

def generate(ws):
    target(name = bin_name("cmd/backend"), command = "true", outputs = ["o"])
    target(name = bin_name("internal/foo/cmd"), command = "true", outputs = ["o"], package = "x")
"#,
    );
    let out = super::generate_with_std(&s, tmp.path(), Some(std_dir())).unwrap();
    let mut names: Vec<_> = out.iter().map(|e| e.wire.name.clone()).collect();
    names.sort();
    assert_eq!(names, vec!["backend".to_string(), "foo".to_string()]);
}

#[test]
fn legacy_giant_alias_still_resolves() {
    // `@giant//` stays a deprecated alias for `@std//`.
    let tmp = TempDir::new().unwrap();
    let s = script(
        tmp.path(),
        "load(\"@giant//go.star\", \"bin_name\")\ndef generate(ws):\n    target(name = bin_name(\"cmd/x\"), command = \"true\", outputs = [\"o\"])\n",
    );
    let out = super::generate_with_std(&s, tmp.path(), Some(std_dir())).unwrap();
    assert_eq!(out[0].wire.name, "x");
}

#[test]
fn loads_stdlib_embedded_without_collection() {
    // With no on-disk collection, `@std//` resolves to the modules compiled
    // into the binary.
    let tmp = TempDir::new().unwrap();
    let s = script(
        tmp.path(),
        "load(\"@std//go.star\", \"bin_name\")\ndef generate(ws):\n    target(name = bin_name(\"cmd/backend\"), command = \"true\", outputs = [\"o\"])\n",
    );
    let out = super::generate_with_std(&s, tmp.path(), None).unwrap();
    assert_eq!(out[0].wire.name, "backend");
}

#[test]
fn unknown_std_module_is_a_clear_error() {
    // Same error whether an on-disk collection is present or not.
    let tmp = TempDir::new().unwrap();
    let s = script(tmp.path(), "load(\"@std//nope.star\", \"x\")\n");
    for dir in [None, Some(std_dir())] {
        let e = super::generate_with_std(&s, tmp.path(), dir).unwrap_err();
        assert!(
            e.to_string().contains("no std module named 'nope.star'"),
            "{e}"
        );
    }
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
    let out = super::generate(&s, tmp.path()).unwrap();
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
    let out = super::generate(&s, root).unwrap();
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
    let out = super::generate(&s, root).unwrap();
    assert_eq!(out[0].wire.command, "module example.com/x\n");
}
