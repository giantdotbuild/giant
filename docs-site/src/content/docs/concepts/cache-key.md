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
   resolved workspace-relative path and content hash. Sorted by path.
   A package-relative input (`src/foo.rs`) is resolved against the
   package directory before hashing, so the hash always sees the same
   workspace-relative path regardless of where the glob was written.
6. **Dep outputs** - for each dependency target, its
   `outputs_content_hash` (the hash-of-hashes of its outputs), NOT its
   cache key. Sorted by hash so dep order in your YAML never shifts the
   key. This is the early-cutoff property (see below). (`giant explain`
   displays this section sorted by dep label for readability - the order
   in the hash itself is by hash value.)

`outputs:` are NOT in the cache key. The recipe determines what
gets built; the recipe's hash determines if we've seen it before.

Neither the workspace name nor the target label is hashed. Two targets
with an identical command, inputs, env, and deps produce the same
cache key - the label does not disambiguate them. If you want two
recipes to cache separately, something in the recipe itself has to
differ.

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
$ giant explain //cmd/server:server
target:      //cmd/server:server
cache key:   3a7f9c4e8b2d1f5e6a8c9d7e4f3b2a1c5d6e9f8a7b4c3d2e1f5a6b7c8d9e
cache state: hit

command:
  go build -o bin/server ./cmd/server
cwd:         //

env (3):
  CGO_ENABLED=0
  GIANT_TARGET_TRIPLE=x86_64-unknown-linux-gnu
  GIANT_VERSION=0.1.0

file inputs (12):
  cmd/server/main.go        sha256:9f3c8d...
  internal/auth/auth.go     sha256:7e2a4b...
  ...

deps (2):
  //proto:gen        sha256:a1b2c3...
  //src/core:core    sha256:d4e5f6...
```

`giant explain` is the first thing to reach for when "why did this
rebuild?" comes up.

### Comparing two breakdowns

When you want to know *what's different* between two targets'
keys - same recipe, different arch flag; same target before/after a
refactor - pass `--diff <other-target>`:

```console
$ giant explain //cmd/server:server --diff //cmd/server:server-debug
comparing:
  -  //cmd/server:server         (3a7f9c4e…)
  +  //cmd/server:server-debug   (8d2b1f4a…)

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

- Target `//proto:gen` depends on `proto/foo.proto`.
- Edit `proto/foo.proto` (cosmetic change - whitespace in a comment).
- `//proto:gen`'s cache key shifts (input content changed) → rebuild.
- But `//proto:gen` produces byte-identical output (`gen/foo.pb.go` is
  the same).
- Downstream `//cmd/server:server` consumes `gen/foo.pb.go`.

`server`'s cache key contribution from `//proto:gen` is
`outputs_content_hash`, NOT `//proto:gen`'s cache key. Since the outputs
are byte-identical, the hash-of-hashes is unchanged. `server`
cache-hits, never re-runs.

This is what makes large monorepos tolerable. Whitespace and comment
edits don't ripple through the dep graph as full rebuilds.

## Toolchain versions

The cache key covers the command, inputs, env, and dependency outputs - but
**not the compiler that runs the command**. Two machines on different Go or
`rustc` versions compute the same key for the same target, and a shared remote
cache will hand one a stale artifact built by the other. So a toolchain
version has to be made part of the key explicitly.

The right way is a **toolchain target**: a `toolchain`-tagged target whose
input is whatever pins your tools (a `devenv.lock` / `flake.lock`, an `asdf`
`.tool-versions`, a checked-in or git-lfs binary) and whose output is a
content-derived identity. Build targets `deps:` on it, so a toolchain bump
re-keys exactly the targets in that ecosystem and leaves the rest cache-warm.
**[Pinning toolchains](/guides/toolchains/)** is the full guide - it covers
devenv/Nix (resolving the store path), git-lfs binaries (hashing the bytes),
per-tool targets, and why a system-installed tool can't be pinned honestly.

The quick-and-dirty alternative is to stamp the version into `env:` so it
folds into the key directly:

```yaml
- name: "server"
  command: "go build -o //bin/server ."
  env:
    GOVERSION: "1.23.4"   # bump this by hand when you bump Go
```

This works but is fragile - you have to remember to bump it, and nothing
checks that the string matches the `go` actually on PATH. Prefer a toolchain
target, which derives the identity from the real tool.
