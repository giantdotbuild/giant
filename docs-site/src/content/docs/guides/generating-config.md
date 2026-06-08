---
title: Generating config
description: Produce giant.yaml files offline with giant gen and the Starlark host.
---

Giant reads **static** config - checked-in `giant.yaml` files, nothing more.
The engine never computes config at build time; it reads the files as they
are. When a repo has too many targets to hand-write (every Go package, every
Dockerfile, every crate), you **generate** the config offline and check it
in, the same way you'd generate any other source.

The tool that runs your generators is the `giant-gen` porcelain - `giant
gen`. It writes static `giant.yaml` files; the engine then reads them via its
normal [package scan](/concepts/packages/), unable to tell generated config
from hand-written. This is the split Bazel uses (the engine reads `BUILD`
files, Gazelle generates them) - discovery is not the engine's job.

`giant gen` runs two kinds of generator: the built-in **Starlark host** (the
integrated path), and **external commands** (any language, via a small
contract). Most repos only need the first.

## Authoring in Starlark

Drop a `giant.star` at the workspace root with a `generate(ws)` entry point.
`giant gen` runs it through the built-in host; it has no separate config to
declare (a root `giant.star` is picked up automatically).

```python
# giant.star - one release build+install target per Rust binary,
# derived from `cargo metadata`.
load("@std//cargo.star", "cargo_targets")

def generate(ws):
    cargo_targets(ws, deps = ["//:devenv"])
```

```console
$ giant gen            # writes the giant.*.yaml files in place
$ giant build //...    # the engine reads what was written
```

Giant ships a standard library of generators - `cargo.star`, `go.star` -
that you `load(...)`. They're plain Starlark built on a generic host: a
`ws` handle (`ws.exec`, `ws.glob`, `ws.read`, `ws.rel`), `parse_json` /
`parse_yaml`, and a `target()` builtin that emits a target. The
language-specific opinion lives in editable Starlark, leaving the engine
generic - `cargo.star` derives targets from `cargo metadata` the same way
`go.star`
derives them from `go list`.

To pin and edit a std generator, vendor it into your repo:

```console
$ giant gen vendor cargo.star      # copies it to star/cargo.star
```

then load it by its repo-local path instead of `@std//`:

```python
load("star/cargo.star", "cargo_targets")
```

## Keeping it fresh: `giant gen --check`

A generated file goes stale when sources change (a new package, a new
import). `giant gen --check` regenerates into a scratch dir and diffs against
what's committed, exiting non-zero if they differ - the staleness gate, built
in. Wire it into CI:

```console
$ giant gen --check
cargo	ok
docker	DRIFT
error: a generator is stale; run `giant gen <name>` and commit the result
```

It reports each generator as `ok`, `DRIFT` (output would change), or
`FAILED`, and exits non-zero if any drifted. This is the check Gazelle
performs with `--mode=diff`, without the shell plumbing.

## External generators (any language)

A generator that isn't Starlark - a Go program, a script, an existing
codegen tool - plugs in as an **external command**, declared in the root
`giant.yaml`'s `generate:` list:

```yaml
generate:
  - go                                   # sugar for { name: go, command: giant-gen-go }
  - { name: docker, command: "./tools/gen-docker.sh" }
  - { script: giant.star, infix: rust } # a Starlark generator, named explicitly
```

A bare name resolves to `giant-gen-<name>` on PATH; a value with a `/` is a
path from the workspace root; anything with spaces runs via `sh -c`. Each
generator owns one filename infix and writes only `giant.<name>.yaml` files.

The **invocation contract**: `giant gen` runs the command with the
workspace root as cwd and two env vars - `GIANT_GEN_OUT` (the directory to
write under, mirroring the source tree) and `GIANT_WORKSPACE` (the root). The
command writes its `giant.<name>.yaml` files and exits 0.

```bash
#!/usr/bin/env bash
# tools/gen-docker.sh - a `docker` generator: one image target per Dockerfile.
set -euo pipefail
find . -name Dockerfile -printf '%h\n' | while read -r dir; do
  mkdir -p "$GIANT_GEN_OUT/$dir"
  cat > "$GIANT_GEN_OUT/$dir/giant.docker.yaml" <<YAML
targets:
  - name: image
    inputs: ["Dockerfile", "**/*"]
    outputs: ["//.build/$(basename "$dir").tar"]
    command: "docker build -t $(basename "$dir") ."
YAML
done
```

`giant gen --check` works for external generators too - it runs them into the
scratch dir and diffs, same as the built-in host.

## Matrices and platforms

"Build for `{arm, x86} × {mac, linux}`, minus a few combinations" is target
multiplication - also generation. The engine has no matrix construct and
never expands one; you write the matrix compactly in your generator (a loop
in Starlark, or whatever your external generator uses) and emit the expanded
targets as ordinary config. The engine only ever sees the result.

## You're never locked in

Because the engine only reads static files, nothing stops you from writing
`giant.yaml` by hand, or with a one-off script you run yourself and commit -
giant can't tell the difference. `giant gen` is the managed path (it owns its
`giant.<name>.yaml` files, checks staleness, and links generated outputs into
the graph); hand-authored config is just config. Most repos use `giant gen`
for the bulk and hand-write the root `giant.yaml` (workspace settings,
toolchains, tasks).
