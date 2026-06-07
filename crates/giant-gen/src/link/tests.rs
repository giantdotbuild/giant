use std::fs;
use std::path::Path;

use giant_schema::Document;
use tempfile::TempDir;

/// Write a file under `root`, creating parent dirs.
fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

/// The deps of target `name` in the document at `rel`.
fn deps_of(root: &Path, rel: &str, name: &str) -> Vec<String> {
    let doc: Document =
        serde_yaml_ng::from_str(&fs::read_to_string(root.join(rel)).unwrap()).unwrap();
    doc.targets
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no target {name} in {rel}"))
        .deps
}

const WS: &str = "workspace:\n  name: w\n";

#[test]
fn generated_consumer_gets_dep_on_generated_producer() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "giant.yaml", WS);
    write(
        root,
        "giant.gen.yaml",
        "targets:\n  \
         - name: producer\n    command: \"true\"\n    outputs: [\"gen/out.txt\"]\n  \
         - name: consumer\n    command: \"true\"\n    inputs: [\"gen/*.txt\"]\n    outputs: [\"c.bin\"]\n",
    );

    assert_eq!(super::run(root).unwrap(), 1);
    assert_eq!(
        deps_of(root, "giant.gen.yaml", "consumer"),
        vec!["//:producer"]
    );
    assert!(deps_of(root, "giant.gen.yaml", "producer").is_empty());
}

#[test]
fn generated_consumer_resolves_handwritten_producer() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(
        root,
        "giant.yaml",
        "workspace:\n  name: w\ntargets:\n  - name: hwprod\n    command: \"true\"\n    outputs: [\"hw/out.txt\"]\n",
    );
    write(
        root,
        "giant.gen.yaml",
        "targets:\n  - name: consumer\n    command: \"true\"\n    inputs: [\"hw/*.txt\"]\n    outputs: [\"c.bin\"]\n",
    );

    let before = fs::read_to_string(root.join("giant.yaml")).unwrap();
    assert_eq!(super::run(root).unwrap(), 1);
    assert_eq!(
        deps_of(root, "giant.gen.yaml", "consumer"),
        vec!["//:hwprod"]
    );
    // Hand-written file is never rewritten.
    assert_eq!(fs::read_to_string(root.join("giant.yaml")).unwrap(), before);
}

#[test]
fn explicit_deps_are_preserved_and_merged() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "giant.yaml", WS);
    write(
        root,
        "giant.gen.yaml",
        "targets:\n  \
         - name: producer\n    command: \"true\"\n    outputs: [\"gen/out.txt\"]\n  \
         - name: consumer\n    command: \"true\"\n    inputs: [\"gen/*.txt\"]\n    outputs: [\"c.bin\"]\n    deps: [\"//:toolchain\"]\n  \
         - name: toolchain\n    command: \"true\"\n    outputs: [\"tc\"]\n",
    );

    super::run(root).unwrap();
    // explicit //:toolchain kept, inferred //:producer added, sorted.
    assert_eq!(
        deps_of(root, "giant.gen.yaml", "consumer"),
        vec!["//:producer", "//:toolchain"]
    );
}

#[test]
fn relink_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "giant.yaml", WS);
    write(
        root,
        "giant.gen.yaml",
        "targets:\n  \
         - name: producer\n    command: \"true\"\n    outputs: [\"gen/out.txt\"]\n  \
         - name: consumer\n    command: \"true\"\n    inputs: [\"gen/*.txt\"]\n    outputs: [\"c.bin\"]\n",
    );

    assert_eq!(super::run(root).unwrap(), 1);
    let after_first = fs::read_to_string(root.join("giant.gen.yaml")).unwrap();
    assert_eq!(super::run(root).unwrap(), 0, "second link rewrites nothing");
    assert_eq!(
        fs::read_to_string(root.join("giant.gen.yaml")).unwrap(),
        after_first
    );
}

#[test]
fn cross_generator_edges_resolve() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "giant.yaml", WS);
    write(
        root,
        "giant.proto.yaml",
        "targets:\n  - name: proto\n    command: \"true\"\n    outputs: [\"gen/api.go\"]\n",
    );
    write(
        root,
        "giant.go.yaml",
        "targets:\n  - name: app\n    command: \"true\"\n    inputs: [\"gen/*.go\"]\n    outputs: [\"app\"]\n",
    );

    super::run(root).unwrap();
    assert_eq!(deps_of(root, "giant.go.yaml", "app"), vec!["//:proto"]);
}

#[test]
fn duplicate_output_is_an_error() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write(root, "giant.yaml", WS);
    write(
        root,
        "giant.gen.yaml",
        "targets:\n  \
         - name: a\n    command: \"true\"\n    outputs: [\"dup\"]\n  \
         - name: b\n    command: \"true\"\n    outputs: [\"dup\"]\n",
    );

    let err = super::run(root).unwrap_err().to_string();
    assert!(err.contains("dup"), "{err}");
}

#[test]
fn is_generated_predicate() {
    assert!(super::is_generated("giant.gen.yaml"));
    assert!(super::is_generated("giant.go.yml"));
    assert!(!super::is_generated("giant.yaml")); // hand-written
    assert!(!super::is_generated("giant.go.bar.yaml")); // dotted infix
    assert!(!super::is_generated("notgiant.gen.yaml"));
}
