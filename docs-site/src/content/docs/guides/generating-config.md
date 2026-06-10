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

To be precise about where the boundary sits: the Starlark interpreter lives
in the `giant-gen` binary and nowhere else. The engine loads YAML, full
stop - it cannot evaluate a `giant.star` any more than `go build` can run
Gazelle. Generation is a step you run when the tree's shape changes (a new
package, a new service), and its output is ordinary config.

**Commit every `giant.<name>.yaml`.** They relate to your `giant.star` the
way generated protobuf stubs relate to their `.proto` files: derived,
checked in, reviewed in diffs. (If you're coming from CMake or autotools,
note the difference: their output is machine-specific and regenerated per
build, while giant's generated config describes the tree, so it belongs in
the repo.) Because the files are committed, a fresh checkout builds with
just the engine and the build porcelain - `giant-gen` doesn't need to be
installed where you only build, CI included. What CI should run instead is
the [drift gate](#keeping-it-fresh-giant-gen---check) below.

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

Giant's standard library of generators - `cargo.star`, `go.star`,
`docker.star`, `controllergen.star`, and whatever lands next - lives in its
own repo, [giantdotbuild/giant-std](https://github.com/giantdotbuild/giant-std),
so it can grow without waiting for a giant release. Each module's usage and
expected output is documented
[in that repo](https://github.com/giantdotbuild/giant-std/tree/main/docs).
They're plain Starlark built on a generic host - the full primitive surface
is in the [Starlark host reference](/reference/starlark/) - and each is
layered the same way: detectors that derive facts from the tree, emitters
that shape one correct target, and a floor that wires them into the common
convention. When a floor doesn't fit your repo, call the detectors and
emitters from your own `giant.star` instead; the floors are a screenful of
Starlark each and read as worked examples.

### Pinning the std collection

`@std//` needs to know which version of the collection you mean. Pin one in
the root `giant.yaml`:

```yaml
std:
  ref: v3          # a giant-std tag or commit sha
  # repo: giantdotbuild/giant-std   (the default; any owner/name works)
```

Each module is fetched once per (repo, ref) and cached under the cache dir,
so generation only touches the network the first time a pin is seen -
after that it runs offline. Bumping `ref` is how you take a new std
version; an unpinned "latest" doesn't exist, since it would make
generation non-reproducible.

Instead of a pin, `path:` points at a local collection directory - a
giant-std checkout, or a path your environment manager (devenv, nix)
provides:

```yaml
std:
  path: ~/Development/giant-std
```

A `GIANT_STD` env var pointing at a directory overrides either form, and
vendored copies (below) sidestep `@std//` entirely.

### Vendoring

To pin a module in-repo and edit it - or to generate fully offline -
vendor it:

```console
$ giant gen vendor cargo.star      # copies it to star/cargo.star
```

then load it by its repo-local path instead of `@std//`:

```python
load("star/cargo.star", "cargo_targets")
```

## Workspace config

The root `giant.yaml` is open at the top level: the engine validates the
sections it owns and ignores keys it doesn't recognise. That makes it the
natural home for your generator's own configuration - a declarative block
your `giant.star` reads back, instead of sidecar config files:

```yaml
# giant.yaml
images:
  registry: registry.example.com/platform
  exclude: [load-tester]
```

```python
def generate(ws):
    cfg = parse_yaml(ws.read("giant.yaml")).get("images", {})
    registry = cfg.get("registry", "registry.local")
    ...
```

Default every field in the generator so an absent block means "the
convention, unmodified", and prefer keys that override the convention (a
curated name, an exclusion) over keys that restate what the tree already
says.

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

This is the one CI job that does need `giant-gen` - and the toolchains your
generators shell out to (`go list`, `cargo metadata`), since `--check`
re-runs them. Build and test jobs run from the committed files and need
neither.

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
