---
title: Structural inputs
description: Hash only the lines you care about.
---

A structural input hashes only the lines of a file matching one of a
list of prefixes. Function-body edits don't shift the cache key; only
the matched lines do. The same idea appears in two places: as the
**excerpt** entry kind in a discovery's `reads` manifest (the
recommended use), and as a `kind: structural` input on a regular
target.

## The motivating problem

Go's `go list -json` is slow on large repos. Most projects enumerate
packages by running it inside a discovery target. The result depends
only on:

- The `package` declarations
- The `import` statements
- A few directives like `//go:embed`

It does NOT depend on function bodies. Hashing every Go file in full
would invalidate the discovery's cache on every cosmetic edit. The
fix is to hash only the lines that actually affect discovery output.

## In discoveries (the recommended use)

A discovery emits an excerpt entry in its `reads.files` manifest:

```jsonc
{
  "targets": [ /* ... */ ],
  "reads": {
    "files": [
      { "path": "go.mod" },
      { "path": "pkg/foo/foo.go", "lines": ["package ", "import ", "//go:embed "] }
    ]
  }
}
```

For each file path, the verifier hashes only the lines whose prefix
matches any pattern in `lines:`. Editing a function body inside
`foo.go` doesn't change the matched-line hash → the discovery's
sidecar still verifies → the cached output is reused without
re-executing the script. See
[Discovery](/concepts/discovery/) for the full cooperative protocol.

## In regular targets

The same algorithm is also available as a first-class input kind on
regular targets:

```yaml
targets:
  - id: "doc:public-api"
    inputs:
      - kind: structural
        files: "**/*.rs"
        lines: ["pub fn ", "pub struct ", "pub enum "]
    outputs: ["docs/api.md"]
    command: "tools/extract-api > docs/api.md"
```

The `files:` can be a string or list. This form is useful for the
small set of non-discovery targets that read source as data
(documentation generators, API surface extractors). **Discovery
targets** may also declare `inputs:` - those file inputs are hashed
into the discovery's cache key, in addition to the recorded-reads
manifest. For most discoveries a `reads.files` excerpt entry is the
better fit, but `inputs:` is accepted, not rejected.

Now, in either form:

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

When the workspace is a git repo, we skip the directory walk entirely.
Giant enumerates candidate files straight from the git index plus any
untracked files, using the `gix` library - no recursive filesystem
walk. It then stat-skips each file against the stored per-file sidecar:
if `(mtime, size)` is unchanged, the file isn't re-read and its
recorded structural hash is reused.

For a 10k-file Go repo with one edited file, this stage answers in a
few milliseconds.

## When to use structural inputs

Use the **excerpt** entry in a discovery's `reads.files` manifest
whenever the discovery script only consults a subset of file
structure. Examples:

- Go discovery: `package`, `import`, `//go:embed`
- TypeScript module resolution: `import`, `export`
- Rust crate discovery: `mod`, `use`, `extern crate`, `pub`

Use the **`kind: structural`** input kind on a regular target when
the target reads source as data - documentation generators, API
surface extractors, dependency-graph dumpers. Niche but real.

Don't use either form for actual compilation - the compiler reads
everything, so the cache key has to reflect everything.

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

For a regular target using `kind: structural`:

```console
$ giant explain doc:public-api | grep -A4 structural
structural_inputs (1):
  **/*.rs (lines: ["pub fn ", "pub struct ", "pub enum "])
    fingerprint: sha256:5c8a3f...
    files scanned: 1284
```

For a discovery target, the per-entry recorded hashes live in the
discovery sidecar under `.giant/discovery/<key>.json`. Open it with
`jq` to see exactly which paths the verifier compares against.
