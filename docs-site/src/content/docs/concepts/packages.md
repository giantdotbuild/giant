---
title: Packages and labels
description: How Giant splits config across the tree and names targets by path.
---

A Giant workspace is a tree of **packages**. A package is any directory
with a `giant.yaml` (or `giant.json`); the file declares the targets that
live in that directory. The engine scans the whole tree, reads every
package file, and merges them into one graph.

```
my-repo/
├── giant.yaml                  # the workspace root (//)
├── crates/
│   ├── giant/giant.yaml        # package //crates/giant
│   └── giant-task/giant.yaml   # package //crates/giant-task
└── docs-site/giant.yaml        # package //docs-site
```

## Labels: `//package:name`

A target's identity is its **label**, derived from where it lives:
`//<package>:<name>`. The package is the file's directory (workspace
relative); the name is the target's local `name:`. A target named
`giant` in `crates/giant/giant.yaml` is `//crates/giant:giant`.

```yaml
# crates/giant/giant.yaml
targets:
  - name: "giant"          #  →  //crates/giant:giant
    command: "cargo build --release -p giant"
    outputs: ["//bin/giant"]
```

The root package is empty, so a target in the root `giant.yaml` is
`//:name`. Names only need to be unique **within their package** - two
packages can both have a `build` target (`//crates/giant:build` and
`//docs-site:build` never collide).

`//crates/giant` is shorthand for `//crates/giant:giant` - a bare package
path means the target whose name matches the last path segment.

## The root config

The root `giant.yaml` is mandatory: it marks the workspace (what `//`
resolves against) and is the only file that may carry workspace-global
settings - `workspace`, `cache`, `remote` - plus the porcelain-owned
`tasks:` / `services:` blocks (read by `giant-task`). A nested package
file carries only `targets:`; putting a `cache:` or `workspace:` in one
is a loud error; it is never silently ignored.

## Package-relative paths

Every path in a config file - `inputs`, `outputs`, `cwd`, the references
that drive dependency inference - resolves relative to its package:

- **Bare = package-relative.** `src/**/*.rs` in `crates/giant/giant.yaml`
  means `crates/giant/src/**/*.rs`.
- **`//` = workspace root.** `//Cargo.lock` is the root file regardless of
  which package references it; `//bin/giant` is a root-level output.
- **`cwd` defaults to the package directory.** Set `cwd: "//"` to run a
  command from the workspace root.
- **No `..`.** Reach another package's files by depending on the target
  that produces them, or with an explicit `//` reference.

So a per-crate package reads its own source with bare globs and the
shared lockfile with `//Cargo.lock`:

```yaml
# crates/giant/giant.yaml
targets:
  - name: "giant"
    inputs:
      - "src/**/*.rs"     # crates/giant/src/**/*.rs
      - "Cargo.toml"      # crates/giant/Cargo.toml
      - "//Cargo.lock"    # the workspace lockfile
    outputs: ["//bin/giant"]
    cwd: "//"
    command: "cargo build --release -p giant && install -m0755 target/release/giant bin/giant"
```

## Glob boundaries

A package's **input** globs stop at a subpackage boundary. A recursive
`inputs: ["**/*.go"]` in package `//src` matches files under `src/`,
**except** any nested package's files - those belong to that package.
This keeps the rule that every file is owned by exactly one package
(its deepest enclosing `giant.yaml`) for the purpose of cache keys, so a
parent target never folds a child's sources into its own key.

The boundary applies to input-glob expansion specifically. Output capture
and affected-detection matching aren't pruned the same way, so don't lean
on a parent `outputs: ["**/*"]` to *avoid* a child's generated files -
scope output globs tightly instead.

## One file or many

Nothing forces you to split. A small project can keep every target in
the root `giant.yaml` (all `//:name`). Splitting earns its keep when a
subdirectory is a natural unit of ownership - a crate, a service, a docs
site - and lets that directory's targets use short, package-relative
paths. For a large tree you usually don't hand-write the package files at
all: a [generator](/guides/generating-config/) writes them.
