---
title: Discovery
description: Materialize targets at config-load time from any source.
---

Discovery is how Giant handles repositories where the target list is
too dynamic to hand-write - every Go package, every Dockerfile, every
Rust crate. A discovery target is a normal subprocess that runs *before*
the main build, emits JSON describing more targets, and tells Giant
exactly which files and directories it consulted so the result can be
cached across runs.

## The shape

```yaml
include:
  - id: "discover:go"
    command: "tools/discover-go.sh > .giant/d/go.json"
    outputs: [".giant/d/go.json"]
    scope: ["."]
```

Three things to notice:

- It's under `include:`, not `targets:`. That's how Giant knows to run
  it during the bootstrap pass before the main build.
- It has **no `inputs:`** field - the loader rejects it. What
  invalidates a discovery's cached output is the `reads` manifest the
  script emits in its JSON (below), not user-declared globs.
- `scope:` is optional. It bounds where the discovery may read from
  (useful when sandboxing is on) and narrows the path-change query
  when fsmonitor is configured.

## The JSON the script writes

```json
{
  "targets": [
    {
      "id": "go:pkg:internal/auth",
      "inputs": ["internal/auth/**/*.go"],
      "outputs": ["bin/auth"],
      "command": "go build -o bin/auth ./internal/auth"
    },
    {
      "id": "go:pkg:cmd/server",
      "inputs": ["cmd/server/**/*.go"],
      "outputs": ["bin/server"],
      "command": "go build -o bin/server ./cmd/server",
      "deps": ["go:pkg:internal/auth"]
    }
  ],
  "reads": {
    "files": [
      { "path": "go.mod" },
      { "path": "go.sum" },
      { "path": "internal/auth/auth.go", "lines": ["package ", "import "] },
      { "path": "cmd/server/main.go",     "lines": ["package ", "import "] }
    ],
    "dirs": [
      { "path": "internal/auth", "filter": "*.go" },
      { "path": "cmd/server",    "filter": "*.go" }
    ]
  }
}
```

Two top-level fields:

- `targets:` - the same target schema as `targets:` in `giant.yaml`.
  Giant merges these into the build graph as if you'd typed them
  yourself.
- `reads:` - the cooperative contract. Lists every file and directory
  the script actually consulted while computing the output. Giant uses
  it on the next run to decide whether the cached output is still
  valid without re-executing the script. If `reads` is missing, the
  output is used once but **not cached** (a warning is logged; opt
  into errors with `discovery.strict: true`).

### Two kinds of file entry

A `reads.files` entry either tells Giant to hash the **whole file** or
to hash a **slice** of it defined by line-prefix patterns:

```jsonc
{ "path": "go.mod" }                                        // whole file
{ "path": "main.go", "lines": "^package " }                 // single prefix
{ "path": "main.go", "lines": ["^package ", "^import "] }   // any of these
```

Excerpt entries are how a Go discovery says "I only care about
`package` and `import` lines in these files." Editing a function body
doesn't change those lines → the recorded hash matches → the
discovery's output is reused without re-running the script.

### Directory entries with optional filter

A `reads.dirs` entry hashes a directory's listing (no recursion):

```jsonc
{ "path": "pkg" }                          // hash all entry names
{ "path": "pkg", "filter": "*.go" }        // only `.go` filenames
{ "path": "pkg", "filter": ["*.go", "*.proto"] }
```

Adding or removing files that match the filter invalidates the entry;
adding a `README.md` to a directory recorded with `filter: "*.go"`
doesn't. Useful when the discovery walks a directory and only cares
about a subset of its children.

## How the bootstrap pass works

1. **Config load.** Giant parses `giant.yaml`, sees the `include:`
   entries, rejects any that declare `inputs:`.
2. **Sidecar lookup.** For each pending discovery, Giant checks
   `.giant/discovery/<key>.json` (key = `cmd + env + cwd + scope`).
   If a sidecar exists and every entry in its `reads` manifest
   matches the live filesystem, the cached output is restored to disk
   and the script doesn't run.
3. **Bootstrap build.** Discoveries with no usable sidecar dispatch
   through the normal build pipeline. Their commands run, the output
   JSON is written, the `reads` manifest is materialized into recorded
   hashes, and a fresh sidecar is saved.
4. **Merge.** Giant reads each discovery's output (cached or freshly
   produced), parses `targets:` and `include:`, and adds them to the
   graph.
5. **Output-based dep inference.** With the full graph in hand, Giant
   walks input/output globs and infers edges between static and
   discovered targets.
6. **Main build.** Proceeds as normal.

The result: a discovery that emits a precise `reads` manifest re-runs
only when the files it actually consulted change. Everything else is
a sub-100ms sidecar verification.

## Writing a cooperative discovery

The script can be in any language; it has to read the workspace, write
JSON to stdout, and include a `reads` manifest of what it consulted.
A worked Go example using `go list -json`:

```bash
#!/usr/bin/env bash
# Emit a Giant discovery fragment for every package in the current
# Go module, plus a `reads` manifest so subsequent runs can skip
# re-execution when nothing relevant changed.
set -euo pipefail

MODULE=$(go list -m)
HAVE_GOSUM=$([ -f go.sum ] && echo 1 || echo 0)

go list -json -deps ./... 2>/dev/null \
  | jq -s \
      --arg module "$MODULE" \
      --arg cwd "$PWD" \
      --argjson have_gosum "$HAVE_GOSUM" '
      map(select(.Module.Path == $module)) as $pkgs
      | ($pkgs | map(. as $p
          | ($p.Dir | sub("^" + $cwd + "/?"; "")) as $reldir
          | (($p.GoFiles // []) + ($p.TestGoFiles // []) + ($p.CgoFiles // []))
          | map(if $reldir == "" then . else $reldir + "/" + . end)
        ) | add // []) as $go_files
      | ($pkgs | map(.Dir | sub("^" + $cwd + "/?"; "")) | map(if . == "" then "." else . end) | unique) as $pkg_dirs
      | {
          targets: ($pkgs | map( { id: "go:pkg:\(.ImportPath | sub("^" + $module + "/?"; ""))",
                                  inputs: [ .Dir + "/**/*.go" ],
                                  command: "go build ./" + (.Dir | sub("^" + $cwd + "/?"; ""))
                                } )),
          reads: {
            files: ([{"path": "go.mod"}]
                   + (if $have_gosum == 1 then [{"path": "go.sum"}] else [] end)
                   + ($go_files | map({path: ., lines: ["package ", "import ", "//go:embed "]}))),
            dirs: ($pkg_dirs | map({path: ., filter: "*.go"}))
          }
        }
'
```

The full example lives in `tests/fixtures/discover-go/tools/discover-go.sh`
in the Giant repo.

## Discovery tools as cached targets

When the discovery tool grows beyond a shell script - a real Go/Rust/Python
binary that does the work - declare a regular target that compiles the
tool, and reference its output as the `command:` of the discovery
entry:

```yaml
targets:
  - id: "build:my-discover"
    inputs:
      - "tools/my-discover/**/*.go"
      - "tools/my-discover/go.mod"
      - "tools/my-discover/go.sum"
    outputs: ["bin/my-discover"]
    command: "cd tools/my-discover && go build -o ../../bin/my-discover ."

include:
  - id: "discover:all"
    deps: ["build:my-discover"]
    command: "./bin/my-discover > .giant/d/all.json"
    outputs: [".giant/d/all.json"]
    scope: ["src/"]
```

The bootstrap picks up `discover:all`, expands `deps:` transitively,
builds `build:my-discover` first (or cache-hits it), then runs the
binary. Three properties fall out for free:

- **The discovery tool is cached like any target.** Edit
  `my-discover/main.go` once → rebuild once → cached forever after.
- **Remote-shareable.** CI machines pull the compiled binary from the
  remote cache, never compile it locally. The compile happens on the
  one machine that warms the cache, then propagates.
- **Source changes invalidate correctly.** Editing the discovery tool's
  source changes its output binary, which appears in the discovery's
  `reads.files` manifest (the tool itself is read on every run), which
  invalidates the discovery's sidecar.

### Why this isn't circular

The shape can feel paradoxical: a target produces something a
discovery uses, and the discovery produces more targets - same graph,
same engine, same caching. It isn't a cycle. There are two distinct
layers:

| | Where it's declared | What it depends on |
|---|---|---|
| **Static layer** (the `targets:` + `include:` entries in `giant.yaml`) | YAML, hand-written | Only paths inside the discovery tool's own source tree |
| **Dynamic layer** (every target the discovery emits) | The discovery's JSON output | Each other, by ID convention |

The static layer's inputs are paths like `tools/my-discover/main.go`
that no dynamic target ever produces. Its cache key can be computed
before any discovery runs. It has no edges into the dynamic layer.

The dynamic layer doesn't override or modify the static layer - it
only adds new target IDs. A discovery emitting a target whose id
collides with a static target is silently skipped, so the static
layer always wins.

### When to use it

- The discovery logic is non-trivial (more than ~50 lines of shell).
- You want type checking, refactoring tools, or unit tests on the
  discovery logic itself.
- The same workspace is built often enough that compiling the tool
  once and caching the binary is cheaper than re-interpreting a
  script on every cold path.

When a shell + jq script is enough, leave it as a shell + jq script.

## What discovery doesn't do

- **It doesn't have access to the engine.** Discovery is a normal
  subprocess. It can't query the cache, can't see what other targets
  exist, can't invoke giant recursively.
- **It doesn't run on every build.** If its `reads` manifest still
  matches the filesystem, the cached output is reused without
  executing the script.
- **It can't declare `inputs:`.** The loader rejects them. The
  recorded-reads manifest is the only invalidation signal.

## Recursive discovery

A discovery target can emit more `include:` entries. The engine's
scheduler processes them like any other discovery: dispatch them
(checking sidecars first), parse outputs, merge new entries onto the
worklist, repeat until the worklist is empty.

```jsonc
// scripts/discover-top.sh writes this.
{
  "include": [
    { "id": "discover:go",     "command": "scripts/discover-go.sh > .giant/d/go.json",     "outputs": [".giant/d/go.json"],     "scope": ["src/"]    },
    { "id": "discover:docker", "command": "scripts/discover-docker.sh > .giant/d/docker.json", "outputs": [".giant/d/docker.json"], "scope": ["deploy/"] }
  ],
  "reads": {
    "files": [{ "path": "scripts/discover-top.sh" }]
  }
}
```

`discover:go` and `discover:docker` run in parallel after the top-level
discovery completes. If either emits more `include:` entries, those
get appended to the same worklist.

A few properties worth knowing:

- **Parallelism.** Independent discoveries in the same round run in
  parallel through the normal executor.
- **Caching.** Each discovery has its own sidecar; warm bootstrap is
  free no matter how deep your discovery tree goes.
- **Cycle safety.** If discovery A emits an include for B, and B
  emits an include for A, the engine notices the duplicate target id
  and silently dedupes - it won't loop.
- **Chain-depth limit.** A discovery that keeps emitting new
  descendants without converging hits a generation cap (currently 8)
  and the build fails with the full chain so you can find the
  runaway. Genuine deep trees don't trip this; runaway emitters do.
- **Coupling between discoveries is just normal target deps.** If
  discovery A needs to read what discovery B produces, declare B's
  output file in A's `reads.files`. Output-based inference wires the
  edge; the executor runs B before A.

## Strict mode

Cooperative discoveries are the recommended pattern, but the default
mode is **lenient**: a discovery without a `reads` manifest works,
just doesn't get cached, and Giant logs a warning. To enforce the
contract:

```yaml
discovery:
  strict: true
```

Any discovery whose output omits `reads` is then a hard error. Useful
in CI to catch new discoveries that forgot to emit the manifest.

## Why discovery is a target

A few projects have asked "why not embed a scripting language for
this?" The short answer is in
[ADR-0001](https://github.com/johnae/giant/blob/main/docs/adr/0001-discovery-as-a-target.md).
The longer answer:

- **Caching falls out automatically.** Discovery is a target whose
  invalidation signal is its own recorded reads - the strictest
  possible "what should re-run when" model.
- **Language-agnostic.** Your discovery script can be bash, Python, Go,
  Rust, whatever you want. Giant doesn't care.
- **Debuggable in isolation.** Run the script directly to see what it
  produces - no Giant runtime in the loop.
- **Tiny core.** No embedded interpreter to ship, version, or sandbox.
