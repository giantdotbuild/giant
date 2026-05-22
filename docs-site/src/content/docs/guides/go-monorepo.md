---
title: Go monorepo
description: A realistic Go monorepo setup - discovery, structural inputs, inferred deps.
---

A complete worked example: a Go monorepo with many packages, discovered
automatically, with the right caching semantics.

## Repo layout

```
.
├── giant.yaml
├── go.mod
├── tools/
│   └── discover-go.sh
├── internal/
│   ├── auth/...
│   └── store/...
├── pkg/
│   └── util/...
└── cmd/
    ├── server/
    └── worker/
```

## `giant.yaml`

```yaml
workspace:
  name: my-monorepo
cache:
  dir: ~/.cache/giant

include:
  - id: "discover:go"
    command: "tools/discover-go.sh > .giant/d/go.json"
    outputs: [".giant/d/go.json"]
    scope: ["."]
```

That's the entire static config. Discovery produces everything else.
Note there's no `inputs:` on the `include:` entry - discoveries are
invalidated by the `reads` manifest they emit (below), not by
declared globs.

## `tools/discover-go.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail

mkdir -p .giant/d

# Collect the per-package Go file paths and dirs as we go so the
# `reads` manifest can list them precisely.
go list -json ./... | jq -s --arg cwd "$PWD" '
  def to_target:
    {
      id: ("go:" + (if .Name == "main" then "bin:" + (.ImportPath | sub(".*/"; "")) else "pkg:" + .ImportPath end)),
      inputs: ([(.Dir + "/*.go")] + (.EmbedPatterns // [] | map(.Dir + "/" + .))),
      outputs: (if .Name == "main" then ["bin/" + (.ImportPath | sub(".*/"; ""))] else [] end),
      command: (
        if .Name == "main"
        then "go build -o bin/" + (.ImportPath | sub(".*/"; "")) + " ./" + .ImportPath
        else "go vet ./" + .ImportPath
        end
      )
    };

  ({
     targets: map(to_target),
     reads: {
       files: (
         [{path: "go.mod"}, {path: "go.sum"}]
         + (map(
             (((.GoFiles // []) + (.TestGoFiles // []) + (.CgoFiles // []))
              | map((.Dir | sub("^" + $cwd + "/?"; "")) + "/" + .))
            )
           | add // []
           | map({path: ., lines: ["package ", "import ", "//go:embed "]}))
       ),
       dirs: (map(.Dir | sub("^" + $cwd + "/?"; "")) | unique
              | map({path: ., filter: "*.go"}))
     }
   })
'
```

The `reads` manifest is what gives the discovery its caching
behavior: `go.mod`, `go.sum`, and every `.go` file's `package` /
`import` / `//go:embed` lines feed the hash. Edit a function body in
any of those `.go` files and the recorded hash doesn't move →
discovery doesn't re-run. Add a new package directory → the parent's
directory listing changes → discovery does re-run.

A real-world script handles more cases (test packages, build tags,
cgo); this is the minimum to see the shape.

## What you get

After `giant build`, the graph looks like:

```
discover:go                       (bootstrap)
  ├─→ go:pkg:internal/auth
  ├─→ go:pkg:internal/store
  ├─→ go:pkg:pkg/util
  ├─→ go:bin:server  (deps via inference: depends on auth, store, util)
  └─→ go:bin:worker  (deps via inference: depends on store, util)
```

The dep arrows from `bin:server` → `pkg:internal/auth` are inferred:
the bin's input glob `cmd/server/*.go` matches the auth package's
output file? No - but auth doesn't produce a file. So inference via
output match doesn't apply here.

How does the dep show up then? Two ways:

1. **The discovery script can emit `deps`** explicitly per target,
   reading them from `go list`'s `Deps` field.
2. **The bin's `inputs:` cover the dep's source files** - if you list
   `internal/**/*.go` as an input on `bin:server`, edits to auth
   re-trigger server. No inference needed; the cache key naturally
   reflects all source.

The second approach is simpler and what most setups use. The first
gives you a cleaner graph in `giant graph`.

## Workflow

```bash
# Full build
giant build

# Just the binaries
giant build 'go:bin:*'

# Skip the database integration tests
giant test --no-tag db

# Watch one binary
giant watch go:bin:server

# What changed since main?
giant affected --base main
```

## Cache behaviour you'll actually see

- **Cold first build:** discovery runs, all packages compile.
- **Edit function body in `internal/auth/auth.go`:** discovery
  cache-hits (no structural change), `pkg:internal/auth` recompiles,
  `bin:server` recompiles (its inputs cover auth source).
- **Edit a comment in `internal/util/format.go`:** depending on your
  inputs spec, the package might cache-hit (if comments aren't visible
  to the compiler-relevant bytes - they're not, but our cache key
  doesn't know that). For Go this is usually a recompile.
- **Add `import "log"` to `internal/auth/auth.go`:** discovery's
  structural input shifts → discovery re-runs → the emitted target
  list might change → graph rebuild → relevant packages recompile.
- **Run `git checkout main`:** discovery cache-hits against the cache
  if you've been on main recently. Most packages cache-hit. Only what
  diverged rebuilds.

## Speed numbers

On a 10k-file Go monorepo with discovery + per-package targets:

| Pass | Time |
|---|---|
| Cold build (everything) | dominated by `go build` |
| Warm no-op | <100 ms |
| One-package edit, downstream rebuild | dominated by `go build` of affected |
| Structural input fingerprint over 10k files (cold) | ~50 ms |
| Same after git status fast-path | ~5 ms |
