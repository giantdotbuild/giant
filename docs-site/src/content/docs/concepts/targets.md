---
title: Targets and inputs
description: The unit of work in Giant and how it composes.
---

Everything Giant does is built around one shape:

```
inputs → command → outputs
```

A target is one instance of that shape. A build graph is a DAG of
targets connected by `deps:` edges - written by hand, or filled in by a
[generator](/guides/generating-config/).

Targets are declared in `giant.yaml` files spread across the tree - each
directory's file declares that directory's package. The engine scans and
merges them into one graph. See [Packages](/concepts/packages/) for how
config is split and how labels are derived.

## Full target schema

```yaml
targets:
  - name: "server"
    inputs:
      - "**/*.go"
      - "//go.mod"
      - "//go.sum"
    outputs: ["//bin/server"]
    deps: []
    cwd: "//"
    command: "go build -o bin/server ./cmd/server"
    env:
      CGO_ENABLED: "0"
    test: false
    tags: ["lang=go", "kind=bin", "release", "linux"]
    cache: true
    remote_cache: true
    sandbox: true
    network: false
    exists: "test -f bin/server"
    timeout_secs: 300
```

This target lives in `cmd/server/giant.yaml`, so its label is
`//cmd/server:server`.

| Field | Meaning |
|---|---|
| `name` | Local name, unique within the package; the engine identity is the `//package:name` label. Required. |
| `inputs` | File globs whose matched files affect the cache key. Package-relative; `//` anchors to the workspace root. |
| `outputs` | Files the command produces, relative to `cwd`. Cached. |
| `deps` | Target dependencies, as labels. Written by hand, or filled in by generation - see [Dependencies](#dependencies). |
| `command` | Shell command, run from `cwd`. Required unless `exists` succeeds. `//` is **not** rewritten here - the shell sees it verbatim. Write paths relative to `cwd`, or use `$GIANT_WORKSPACE_ROOT` / `$GIANT_PACKAGE_DIR` (see [below](#environment-giant-sets-for-every-command)). |
| `cwd` | Working directory. Default: the package directory. `//` anchors to the workspace root. |
| `env` | Environment variables. Hashed into the cache key. |
| `test` | Marks this as a test target. `giant test` runs only these. |
| `tags` | Free-form labels (`lang=go`, `kind=bin`, …) for `--tag` / `--no-tag` filtering. |
| `cache` | Set to `false` to never cache this target's outputs. |
| `remote_cache` | Set to `false` to exclude from remote cache uploads. |
| `sandbox` | Set to `false` to exempt this target when a run is sandboxed (`--sandbox`, `giant verify`). Default `true`. Plain runs are never sandboxed regardless. |
| `network` | Set to `true` to grant network access when sandboxed. Default `false`. |
| `exists` | External check; if it succeeds, the command is skipped. |
| `timeout_secs` | Seconds before the command is killed. Default: unlimited. |

Language and kind are not part of the identity - they live in `tags:`
as `lang=go`, `kind=bin`, and so on. The label is purely path-derived.

## Inputs

Two input shapes:

### File globs (the common case)

```yaml
inputs:
  - "src/**/*.rs"   # this package's own source
  - "//Cargo.lock"  # the root lockfile, anchored to the workspace
```

Standard glob semantics. `**` matches directories recursively; `*` does
not cross `/`. Paths are package-relative by default - `src/**/*.rs`
resolves under the directory holding this `giant.yaml`. A leading `//`
anchors to the workspace root, so a crate package can reach the root
lockfile without walking up with `../`.

Every matched file's content hash contributes to the cache key.

### Output references

When target B's input glob matches a file target A produces, B needs
`deps:` naming A so A runs first. In hand-written config you declare
that edge yourself; in [generated](/guides/generating-config/) config
the link pass finds the match and writes the `deps:` line for you. See
[Dependencies](#dependencies) below.

## Outputs

Each `outputs:` entry is a **glob**, expanded after the command runs;
every matching file is captured. A literal path is the degenerate case -
it matches itself or nothing, so a named output still has the must-exist
contract (if the command didn't produce it, the pattern matches nothing
and the build fails). A pattern that matches zero files is an error.
Outputs are relative to the target's `cwd`.

```yaml
outputs:
  - "bin/server"           # must exist (a named output)
  - "internal/store/*.go"  # capture every generated file
```

Named and glob entries **compose**: keep naming the files that must
exist, and add a glob for codegen output whose names you can't enumerate
(`sqlc generate`, `buf generate`, …). Use a recursive glob like
`gen/**/*.go` for a whole tree. Globs are loose - Giant captures and
restores the matched set but never deletes other files. (A directory you
*own* and want pruned to match exactly is a separate, deferred feature.)

After the command runs, Giant, for every matched file:

1. Reads it and computes its SHA-256.
2. Stores the bytes in the content-addressed store.
3. Records the path + hash + mode in an action-cache entry.

The sorted set of (path, hash) folds into the `outputs_content_hash` that
dependents key on - so a change to any generated file rebuilds them. On a
cache hit, Giant restores the recorded set from CAS; no command runs.

### Targets with no outputs

A target can have an empty `outputs:` list. Such targets only run for
side effects (e.g. linting, a `docker push`). Their cache hit means
"the inputs and env are unchanged since the last successful run."

Declare outputs honestly: an output must be the artifact dependents
actually consume. A marker file standing in for bigger state (say,
`node_modules/.package-lock.json` for a whole `node_modules/`) works on
the machine that built it, but under a shared cache another machine
restores the marker and none of the state it implies. For external
state like that, use `cache: false` with an [`exists`
check](#the-exists-escape-hatch) against the real thing instead.

```yaml
- name: "lint"
  inputs: ["**/*.go"]
  outputs: []
  tags: ["lang=go", "kind=lint"]
  command: "golangci-lint run ./..."
```

## Dependencies

The engine reads `deps:` exactly as written and never invents an edge.
What varies is who writes the line.

### By hand

At five or ten targets, the edges are the easy part of the config - you
know which target feeds which, and you say so:

```yaml
# proto/giant.yaml
- name: "gen"
  inputs: ["**/*.proto"]
  outputs: ["//gen/api.pb.go"]
  command: "..."

# cmd/server/giant.yaml
- name: "server"
  inputs: ["**/*.go", "//gen/**/*.go"]
  outputs: ["//bin/server"]
  deps: ["//proto:gen"]
  cwd: "//"
  command: "go build -o bin/server ./cmd/server"
```

`deps:` is also how you order targets that share no file at all - the
upstream produces nothing the downstream reads:

```yaml
- name: "production"
  inputs: []
  outputs: []
  cache: false
  deps: ["//docker:api", "//docker:worker"]
  command: "kubectl apply -f k8s/"
```

### Filled in by generation

Hundreds of targets is where maintaining edges by hand stops scaling,
and that's exactly where [generation](/guides/generating-config/) takes
over. After generators emit, a **link pass** resolves every target's
input globs against every target's outputs and writes the matches into
the generated files as ordinary `deps:` lines - in the example above,
`//gen/api.pb.go` matching `//gen/**/*.go` becomes a written-down
`deps: ["//proto:gen"]` in the generated `cmd/server` file.

Because this happens offline, the inferred edge is visible: a committed
line you read in code review, with `giant gen --check` keeping it from
drifting. The link pass reads hand-written targets as producers too (a
generated target consuming a hand-written target's output gets its
edge), but it only ever writes into generated files - hand-written
targets keep their `deps:` exactly as you authored them.

## The `exists` escape hatch

Some commands are expensive to dry-run but cheap to check. The
canonical example is Docker:

```yaml
- name: "api"
  inputs: ["Dockerfile", "src/**/*"]
  outputs: []
  cache: false
  tags: ["kind=image"]
  exists: "docker image inspect example/api:$GIANT_CACHE_KEY >/dev/null 2>&1"
  command: "docker build -t example/api:$GIANT_CACHE_KEY ."
```

Before running `command`, Giant runs `exists`. If `exists` exits 0,
the command is skipped - Giant treats the target as already produced.
This lets you cache against an external system (Docker daemon, a remote
registry) without storing the image bytes in Giant's local cache.

`GIANT_CACHE_KEY` (the hex cache key) is provided in the environment
when `exists` runs - see below.

## Environment Giant sets for every command

Giant injects a few variables into the environment of every `command`
(and every `exists` check). Reach for these instead of `//` inside a
command, which the shell does not rewrite:

| Variable | Value |
|---|---|
| `GIANT_WORKSPACE_ROOT` | Absolute path to the workspace root. Write a root-anchored output as `$GIANT_WORKSPACE_ROOT/bin/server` rather than fighting `//`. |
| `GIANT_PACKAGE_DIR` | Absolute path to this target's package directory (where its `giant.yaml` lives). Equal to the default `cwd`, so it still points at the package even when you set `cwd: "//"`. |
| `GIANT_CACHE_KEY` | The target's hex cache key. Handy for tagging an external artifact by Giant's identity, e.g. `docker build -t img:$GIANT_CACHE_KEY .`. |

Your `env:` map is applied after these and can override any of them.
These are the two equivalent ways to land a binary in `//bin/`:

```yaml
# (a) act from the root
cwd: "//"
command: "go build -o bin/server ./cmd/server"

# (b) stay in the package, anchor the output explicitly
command: "go build -o $GIANT_WORKSPACE_ROOT/bin/server ."
```

## Test targets

Add `test: true` and the target only runs under `giant test`. The
default `giant build` excludes them.

```yaml
- name: "auth"
  inputs: ["**/*.go"]
  outputs: ["test-cache/auth.ok"]
  test: true
  tags: ["lang=go", "kind=test"]
  command: "go test . && touch test-cache/auth.ok"
```

Tests are normal targets - cached the same way as build targets,
selected via the same patterns, run in parallel.
