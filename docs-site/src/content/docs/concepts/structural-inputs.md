---
title: Structural inputs
description: Only the lines you care about contribute to the cache key.
---

A structural input is an input declaration that only hashes lines
matching one of a list of prefixes. Function-body edits don't shift
the cache key; only structural changes do.

## The motivating problem

Go's `go list -json` is slow on large repos. Most projects discover
packages by running it inside a discovery target. But its output
depends only on:

- The `package` declarations
- The `import` statements
- A few directives like `//go:embed`

It does NOT depend on function bodies. If you declare `**/*.go` as a
plain input to your discovery target, every edit anywhere in any Go
file invalidates the cache, even cosmetic edits to function bodies.

Structural inputs fix this:

```yaml
include:
  - id: "discover:go"
    inputs:
      - "go.mod"
      - kind: structural
        files: "**/*.go"
        lines: ["package ", "import ", "//go:embed "]
    outputs: [".giant/d/go.json"]
    command: "tools/discover-go.sh > .giant/d/go.json"
```

Now:

- Edit a function body → no rebuild.
- Add `import "log"` → rebuild.
- Rename a package → rebuild.

## How the hash is computed

For each file matched by `files:`:

1. Read it line by line.
2. Keep only lines starting with one of the `lines:` prefixes.
3. Hash that filtered subset.

The whole input's fingerprint is the hash-of-hashes across all files,
sorted by path.

## The three-stage fast path

Naively, structural inputs require reading every matched file every
time you check the cache key. For a monorepo with 10k Go files, that's
10k file reads per build - slow even on SSD.

Giant uses a three-stage approach:

### Stage 1: cold compute

First time we see a target, walk the filesystem, read every file,
compute the structural hash. Write a sidecar JSON to the cache
recording per-file `(mtime, size, structural_hash)`.

### Stage 2: mtime-skip warm validation

On the second build, walk the filesystem again - but for each file,
compare `(mtime, size)` against the sidecar. If unchanged, reuse the
recorded structural hash without re-reading the file.

This is the common case. mtime checks are fast; we never open most
files.

### Stage 3: git fast-path

When the workspace is a git repo, we skip the filesystem walk
entirely for tracked-unmodified files. `git status --porcelain` tells
us which files are modified; those are the only ones we need to
revalidate.

For a 10k-file Go repo with one edited file, this stage answers in a
few milliseconds.

## When to use structural inputs

Use them whenever your command only cares about a subset of file
structure. Examples:

- Go discovery: `package`, `import`, `//go:embed`
- TypeScript module resolution: `import`, `export`
- Rust crate discovery: `mod`, `use`, `extern crate`, `pub`
- Linting that only cares about declarations, not bodies

Don't use them for actual compilation - the compiler reads everything,
so the cache key has to reflect everything.

## Tradeoffs

- **Soundness.** If you list `package` as a prefix but forget that
  `//go:build` directives also affect package selection, you'll miss
  cache invalidations. Adding a prefix later is safe; removing one
  isn't.
- **Sidecar storage.** The per-target sidecar is ~50 bytes per file
  scanned. 10k files = 500 KB on disk. Negligible.
- **First-build cost.** Stage 1 still has to read every file once.
  Subsequent builds are fast.

## Inspecting a structural input

```console
$ giant explain discover:go | grep -A4 structural
structural_inputs (1):
  **/*.go (lines: ["package ", "import ", "//go:embed "])
    fingerprint: sha256:5c8a3f...
    files scanned: 8423
```
