---
title: Pinning toolchains
description: Make the compiler part of the cache key so a toolchain upgrade invalidates the right targets - and no more.
---

A target's cache key covers its command, inputs, environment, and the
output hashes of its dependencies. It does **not** automatically cover
the compiler that runs the command. So if one machine has Go 1.22 and
another has Go 1.23, they compute the same key for the same target and
can share a cache entry that isn't really interchangeable. A shared
remote cache turns that into silent poisoning: upgrade `rustc` on one
machine and every other machine pulls a stale artifact.

Giant has no built-in notion of a toolchain. Instead, you model one as
an ordinary target, and the targets that use it depend on it. The
existing cache machinery does the rest.

## A toolchain is a target

A toolchain target is a normal, statically-declared target: it lives in
a `giant.yaml` like everything else (hand-written, or generated offline
and checked in). It declares an input that changes when the toolchain
changes, runs a command that writes a content-derived identity, and
carries the `toolchain` tag. The dogfood keeps one at the workspace
root, `//:devenv`, but the [package](/concepts/packages/) is up to you -
`//toolchain:rust` works just as well. Its label is what build targets
`deps:` on:

```yaml
# giant.yaml (workspace root)
targets:
  - name: "rust"               #  →  //:rust
    inputs: ["//devenv.lock", "//devenv.nix"]
    cwd: "//"
    command: "command -v rustc | xargs readlink -f > .giant/toolchains/rust.id"
    outputs: ["//.giant/toolchains/rust.id"]
    tags: ["toolchain"]
```

```yaml
# crates/server/giant.yaml
targets:
  - name: "server"             #  →  //crates/server:server
    inputs: ["src/**/*.rs"]
    cwd: "//"
    command: "cargo build -p server --release && install -m755 target/release/server bin/server"
    outputs: ["//bin/server"]
    deps: ["//:rust"]
```

`//crates/server:server`'s key folds in `//:rust`'s output hash. Change
the Rust toolchain and the id file's content changes, which re-keys the
build target. The id file lives under `.giant/` because it's generated
state you never commit - only its content hash matters. The target runs
with `cwd: "//"` so the bare `.giant/...` it writes and the `//.giant/...`
it declares as an output both resolve to the same place at the workspace
root, regardless of which package the target lives in. (`//` is rewritten
in `inputs`/`outputs`/`cwd`, but **not** inside `command` - so the command
writes a path relative to its `cwd`, not a `//`-anchored one.)

This is the same shape Bazel and Buck2 use: the toolchain is a node in
the dependency graph, so a toolchain change re-keys exactly the targets
that depend on it. A Node bump moves `//:node`'s id and leaves `//:rust`
untouched, so your Rust targets stay cached. You get per-ecosystem
scoping for free.

## With devenv

If you pin your tools with [devenv](https://devenv.sh) (or plain Nix),
the cleanest identity is the resolved store path of the executable:

```yaml
# giant.yaml (workspace root)
- name: "go"                   #  →  //:go
  inputs: ["//devenv.lock", "//devenv.nix"]
  cwd: "//"
  command: "command -v go | xargs readlink -f > .giant/toolchains/go.id"
  outputs: ["//.giant/toolchains/go.id"]
  tags: ["toolchain"]
```

`command -v go` finds `go` on PATH; `readlink -f` resolves it to its
store path, something like `/nix/store/9x…-go-1.22.1/bin/go`. That path
is derived from the toolchain's whole build recipe, so it moves whenever
the toolchain definition changes. The engine just hashes the string - it
has no idea Nix is involved.

The soundness rests on devenv's own guarantee: if `devenv.lock` hasn't
changed, the realized `go` hasn't changed, so the toolchain target stays
cached and doesn't even re-run. The trust boundary is devenv's, the same
way it trusts your declared inputs.

One caveat worth knowing: Nix store paths are derived from build
*inputs*, not output bytes. So the path can change even when the binary
is identical (rebuilt from a different but irrelevant input). That only
ever over-invalidates - it never reuses a stale artifact - so it's the
safe direction. If you want an exact-content identity, use the
`sha256sum` form below.

### One toolchain target per tool

Write one toolchain target per tool you pin, each carrying the
`toolchain` tag, then stamp `deps: ["//:<tool>"]` on the build targets in
that ecosystem:

```yaml
# giant.yaml (workspace root)
targets:
  - name: "go"                 #  →  //:go
    inputs: ["//devenv.lock", "//devenv.nix"]
    cwd: "//"
    command: "command -v go | xargs readlink -f > .giant/toolchains/go.id"
    outputs: ["//.giant/toolchains/go.id"]
    tags: ["toolchain"]

  - name: "node"               #  →  //:node
    inputs: ["//devenv.lock", "//devenv.nix"]
    cwd: "//"
    command: "command -v node | xargs readlink -f > .giant/toolchains/node.id"
    outputs: ["//.giant/toolchains/node.id"]
    tags: ["toolchain"]
```

Every Go target depends on `//:go`; every Node target on `//:node`. A
Node bump moves `//:node`'s id and leaves the Go toolchain untouched, so
your Go targets stay cached. (Prefer a dedicated package? Put them in
`toolchain/giant.yaml` and the labels become `//toolchain:go` /
`//toolchain:node` - same mechanism, different home.)

## With checked-in or git-lfs binaries

If a tool lives in the repo at a fixed path - say `bin/go` tracked by
git-lfs - the resolved-path trick does **not** work. The path
(`bin/go`) is stable while the bytes change, so a path-based identity
never moves and you'd reuse a stale artifact. Hash the content instead:

```yaml
# giant.yaml (workspace root)
- name: "go"                   #  →  //:go
  inputs: ["//bin/go"]
  cwd: "//"
  command: "sha256sum bin/go | cut -d' ' -f1 > .giant/toolchains/go.id"
  outputs: ["//.giant/toolchains/go.id"]
  tags: ["toolchain"]
```

`inputs: ["//bin/go"]` makes the target re-run only when the binary
changes; the id file holds the content digest. This works whether the
working tree has the real binary (its bytes are hashed) or just the
git-lfs pointer (the pointer file contains the content's `oid`, which
moves with the binary).

The rule across both cases: the identity must change when the toolchain
changes. A resolved store path satisfies that; a content digest
satisfies that; a bare path does not. The engine hashes whatever the
command writes, so getting this right is on you - `giant explain` shows
each toolchain dependency's resolved hash so you can confirm it moves
when you expect.

## Showing toolchain targets

Toolchain targets are folded out of the default output so the view stays
focused on your build. They still build, and a failing toolchain target
always surfaces. To see them:

```bash
giant build --show-toolchains
```

The same flag works on `giant test` and on `--watch` (`build --watch` / `test --watch`).

## System-installed tools

A toolchain target needs an input that records the tool's version. If
you rely on a system-installed compiler with nothing pinning it - no
lockfile, no checked-in binary - there's no honest input to declare, and
the toolchain target can't tell when it changed. This is unsupported
rather than blocked: you can point a target at a system tool, but it
won't invalidate correctly. Pin your toolchain with devenv, a lockfile,
or a checked-in binary, and the patterns above keep your cache honest.
