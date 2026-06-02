---
title: Targets and inputs
description: The unit of work in Giant and how it composes.
---

Everything Giant does is built around one shape:

```
inputs → command → outputs
```

A target is one instance of that shape. A build graph is a DAG of
targets connected by inferred or explicit dependencies.

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
| `deps` | Explicit target dependencies (most are inferred - see below). |
| `command` | Shell command, run from `cwd`. Required unless `exists` succeeds. `//` is **not** rewritten here - the shell sees it verbatim. Write paths relative to `cwd` (set `cwd: "//"` to act from the workspace root). |
| `cwd` | Working directory. Default: the package directory. `//` anchors to the workspace root. |
| `env` | Environment variables. Hashed into the cache key. |
| `test` | Marks this as a test target. `giant test` runs only these. |
| `tags` | Free-form labels (`lang=go`, `kind=bin`, …) for `--tag` / `--no-tag` filtering. |
| `cache` | Set to `false` to never cache this target's outputs. |
| `remote_cache` | Set to `false` to exclude from remote cache uploads. |
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

### Output references (inferred deps)

You don't write these explicitly - Giant infers them. If target B's
input glob matches target A's output file, B automatically depends on
A.

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

```yaml
- name: "lint"
  inputs: ["**/*.go"]
  outputs: []
  tags: ["lang=go", "kind=lint"]
  command: "golangci-lint run ./..."
```

## Dependencies

Two flavors:

### Inferred (the common case)

If target B's `inputs:` glob matches a file produced by target A's
`outputs:`, B depends on A. Giant works this out at graph-build time
by walking the cross-product.

```yaml
# proto/giant.yaml
- name: "gen"
  inputs: ["**/*.proto"]
  outputs: ["//gen/api.pb.go"]
  tags: ["kind=gen"]
  command: "..."

# cmd/server/giant.yaml
- name: "server"
  inputs: ["**/*.go", "//gen/**/*.go"]
  outputs: ["//bin/server"]
  tags: ["lang=go", "kind=bin"]
  cwd: "//"
  command: "go build -o bin/server ./cmd/server"
  # `deps: ["//proto:gen"]` is inferred - //gen/api.pb.go matches //gen/**/*.go.
```

### Explicit

Use `deps:` when there's a dependency Giant can't infer - usually
because the upstream target produces no file the downstream target
reads:

```yaml
- name: "production"
  inputs: []
  outputs: []
  cache: false
  deps: ["//docker:api", "//docker:worker"]
  command: "kubectl apply -f k8s/"
```

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
when `exists` runs.

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
