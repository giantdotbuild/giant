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

## Full target schema

```yaml
targets:
  - id: "go:bin:server"
    inputs:
      - "cmd/server/**/*.go"
      - "internal/**/*.go"
      - kind: structural
        files: "internal/**/*.go"
        lines: ["package ", "import "]
    outputs: ["bin/server"]
    deps: []
    command: "go build -o bin/server ./cmd/server"
    cwd: ""
    env:
      CGO_ENABLED: "0"
    test: false
    tags: ["release", "linux"]
    cache: true
    remote_cache: true
    exists: "test -f bin/server"
    timeout: 300
```

| Field | Meaning |
|---|---|
| `id` | Unique target name. Required. |
| `inputs` | Files (globs) and/or structural inputs that affect the cache key. |
| `outputs` | Files the command produces, relative to `cwd`. Cached. |
| `deps` | Explicit target dependencies (most are inferred - see below). |
| `command` | Shell command. Required unless `exists` succeeds. |
| `cwd` | Working directory, workspace-relative. Default: workspace root. |
| `env` | Environment variables. Hashed into the cache key. |
| `test` | Marks this as a test target. `giant test` runs only these. |
| `tags` | Free-form labels for `--tag` / `--no-tag` filtering. |
| `cache` | Set to `false` to never cache this target's outputs. |
| `remote_cache` | Set to `false` to exclude from remote cache uploads. |
| `exists` | External check; if it succeeds, the command is skipped. |
| `timeout` | Seconds before the command is killed. Default: unlimited. |

## Inputs

Three input shapes:

### File globs (the common case)

```yaml
inputs:
  - "src/**/*.go"
  - "go.mod"
  - "go.sum"
```

Standard glob semantics. `**` matches directories recursively; `*` does
not cross `/`. Patterns are matched against workspace-relative paths.

Every matched file's content hash contributes to the cache key.

### Structural inputs

```yaml
inputs:
  - kind: structural
    files: "internal/**/*.go"
    lines: ["package ", "import ", "//go:embed "]
```

Only lines starting with one of the listed prefixes contribute to the
hash. Function-body edits don't invalidate the cache. The full story
is on the [Structural inputs](/concepts/structural-inputs/) page.

### Output references (inferred deps)

You don't write these explicitly - Giant infers them. If target B's
input glob matches target A's output file, B automatically depends on
A. See [the discovery page](/concepts/discovery/) for how this composes
with discovery-generated targets.

## Outputs

Outputs are files (not directories). Relative to the target's `cwd`.

```yaml
outputs:
  - "bin/server"
  - "dist/manifest.json"
```

After the command runs, Giant:

1. Reads each output file.
2. Computes its SHA-256.
3. Stores the bytes in the content-addressed store.
4. Records the path + hash + mode in an action-cache entry.

On a cache hit, Giant reads the AC entry and writes the bytes back
from CAS - no command runs.

### Targets with no outputs

A target can have an empty `outputs:` list. Such targets only run for
side effects (e.g. linting, a `docker push`). Their cache hit means
"the inputs and env are unchanged since the last successful run."

```yaml
- id: "lint:go"
  inputs: ["**/*.go"]
  outputs: []
  command: "golangci-lint run ./..."
```

## Dependencies

Two flavors:

### Inferred (the common case)

If target B's `inputs:` glob matches a file produced by target A's
`outputs:`, B depends on A. Giant works this out at graph-build time
by walking the cross-product.

```yaml
- id: "proto:gen"
  inputs: ["api/**/*.proto"]
  outputs: ["gen/api.pb.go"]
  command: "..."

- id: "go:bin:server"
  inputs: ["cmd/server/**/*.go", "gen/**/*.go"]
  outputs: ["bin/server"]
  command: "go build -o bin/server ./cmd/server"
  # `deps: ["proto:gen"]` is inferred - gen/api.pb.go matches gen/**/*.go.
```

### Explicit

Use `deps:` when there's a dependency Giant can't infer - usually
because the upstream target produces no file the downstream target
reads:

```yaml
- id: "deploy:production"
  inputs: []
  outputs: []
  cache: false
  deps: ["docker:api", "docker:worker"]
  command: "kubectl apply -f k8s/"
```

## The `exists` escape hatch

Some commands are expensive to dry-run but cheap to check. The
canonical example is Docker:

```yaml
- id: "docker:api"
  inputs: ["Dockerfile", "src/**/*"]
  outputs: []
  cache: false
  exists: "docker image inspect example/api:$INPUTS_HASH >/dev/null 2>&1"
  command: "docker build -t example/api:$INPUTS_HASH ."
```

Before running `command`, Giant runs `exists`. If `exists` exits 0,
the command is skipped - Giant treats the target as already produced.
This lets you cache against an external system (Docker daemon, a remote
registry) without storing the image bytes in Giant's local cache.

`INPUTS_HASH` is provided in the environment when `exists` runs.

## Test targets

Add `test: true` and the target only runs under `giant test`. The
default `giant build` excludes them.

```yaml
- id: "go:test:auth"
  inputs: ["internal/auth/**/*.go"]
  outputs: ["test-cache/auth.ok"]
  test: true
  command: "go test ./internal/auth && touch test-cache/auth.ok"
```

Tests are normal targets - cached the same way as build targets,
selected via the same language, run in parallel.
