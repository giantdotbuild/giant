---
title: The cache key
description: What goes into the hash, what doesn't, and why.
---

The cache key is a SHA-256 hash computed from everything that can
legitimately change a target's output. If two builds produce the same
key, they produce the same outputs. If anything in the recipe shifts,
the key shifts, and Giant rebuilds.

## What's in the hash

Composed in this order (the exact byte stream matters for
reproducibility):

1. **Schema version marker** - a leading version tag, so a change to
   the key layout invalidates old entries deterministically.
2. **The command** - verbatim. Changing `go build` to `go build -trimpath`
   changes the key.
3. **The cwd** - workspace-relative path.
4. **Env vars** - the target's `env:`, sorted by name, plus two
   built-ins Giant always sets: `GIANT_TARGET_TRIPLE` and
   `GIANT_VERSION`.
5. **File inputs** - for every file matched by an input glob, its
   workspace-relative path and content hash. Sorted by path.
6. **Structural inputs** - fingerprint hash for each structural input
   (see [Structural inputs](/concepts/structural-inputs/)).
7. **Dep outputs** - for each dependency target, its
   `outputs_content_hash` (the hash-of-hashes of its outputs), NOT its
   cache key. Sorted by dep ID. This is the early-cutoff property (see
   below).

`outputs:` are NOT in the cache key. The recipe determines what
gets built; the recipe's hash determines if we've seen it before.

Neither the workspace name nor the target ID is hashed. Two targets
with an identical command, inputs, env, and deps produce the same
cache key - the ID does not disambiguate them. If you want two
recipes to cache separately, something in the recipe itself has to
differ.

### Discovery targets

Discovery targets (`include:` entries) use a separate composition:

1. **The command, cwd, env, scope** - as above.
2. **Content inputs** - the union of (a) every argv token in the
   command that resolves to a real file in the workspace (including
   `$PATH` lookups whose target lives in-tree, e.g.
   `bin/my-discover`), and (b) any files matched by a declared
   `inputs:` glob. Deduped on workspace-relative path so a file
   caught both ways contributes once.

No file inputs from the regular schema, no dep cache keys - the
recorded-reads manifest (see [Discovery](/concepts/discovery/)) is
the warm-path verifier, and dep edges from declared `deps:` order
execution without leaking into the discovery's own key.

## What's NOT in the hash

- **The current time, current user, current host.** Two users on two
  machines running the same command on the same inputs get the same
  cache key.
- **Output file paths.** Changing where outputs land doesn't shift the
  key (but it does change the recipe - adjust thoughtfully).
- **Comments in your config file.** Giant parses the YAML; whitespace
  and comments are normalized away.
- **The order of inputs in your YAML.** Inputs are sorted before
  hashing.

## Inspecting a key

```console
$ giant explain go:bin:server
target:      go:bin:server
key:         3a7f9c4e8b2d1f5e6a8c9d7e4f3b2a1c5d6e9f8a7b4c3d2e1f5a6b7c8d9e
cwd:         <workspace root>

env (3):
  CGO_ENABLED=0
  GIANT_TARGET_TRIPLE=x86_64-unknown-linux-gnu
  GIANT_VERSION=0.1.0

file_inputs (12):
  cmd/server/main.go        sha256:9f3c8d...
  internal/auth/auth.go     sha256:7e2a4b...
  ...

structural_inputs (1):
  internal/**/*.go (lines: ["package ", "import "])
    fingerprint: sha256:5c8a3f...

dep_outputs (2):
  proto:gen          sha256:a1b2c3...
  rust:lib:core      sha256:d4e5f6...
```

`giant explain` is the first thing to reach for when "why did this
rebuild?" comes up.

### Comparing two breakdowns

When you want to know *what's different* between two targets'
keys - same recipe, different arch flag; same target before/after a
refactor - pass `--diff <other-target>`:

```console
$ giant explain go:bin:server --diff go:bin:server-debug
comparing:
  -  go:bin:server         (3a7f9c4e…)
  +  go:bin:server-debug   (8d2b1f4a…)

── command ──
  - go build -o bin/server ./cmd/server
  + go build -gcflags='all=-N -l' -o bin/server-debug ./cmd/server

── env (user) ──
  - CGO_ENABLED=0
  + CGO_ENABLED=1
```

Identical fields are suppressed. If the keys match, you get a
"cache keys are identical" line and nothing else.

## Early cutoff

A subtle but valuable property: an upstream rebuild doesn't always
invalidate downstream.

Scenario:

- Target `proto:gen` depends on `api/foo.proto`.
- Edit `api/foo.proto` (cosmetic change - whitespace in a comment).
- `proto:gen`'s cache key shifts (input content changed) → rebuild.
- But `proto:gen` produces byte-identical output (`gen/foo.pb.go` is
  the same).
- Downstream `go:bin:server` consumes `gen/foo.pb.go`.

`server`'s cache key contribution from `proto:gen` is
`outputs_content_hash`, NOT `proto:gen`'s cache key. Since the outputs
are byte-identical, the hash-of-hashes is unchanged. `server`
cache-hits, never re-runs.

This is what makes large monorepos tolerable. Whitespace and comment
edits don't ripple through the dep graph as full rebuilds.

## Toolchain versions

If your build's behaviour depends on a toolchain version (Go's
compiler, Node's interpreter, etc.), put it in `env:` so the cache key
reflects it:

```yaml
- id: "go:bin:server"
  command: "go build -o bin/server ./cmd/server"
  env:
    GOVERSION: "1.23.4"   # bump this when you bump Go
```

Alternatively, derive it on the fly via a discovery target:

```yaml
include:
  - id: "discover:toolchain-versions"
    command: "tools/get-versions.sh > .giant/d/toolchains.json"
    outputs: [".giant/d/toolchains.json"]
    scope: ["."]
```

The discovery's script reads `go.mod` (and whatever else) and emits
that in its `reads.files` manifest, so changes to the pinned version
re-run the discovery automatically.

And reference the resulting target IDs as deps. When the version
changes, the discovery target's outputs shift, the cache keys shift,
everything downstream rebuilds.
