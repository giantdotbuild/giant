---
title: Discovery
description: Materialize targets at config-load time from any source.
---

Discovery is how Giant handles repositories where the target list is
too dynamic to hand-write - every Go package, every Dockerfile, every
Rust crate. A discovery target is a normal target that runs *before*
the main build and emits JSON describing more targets to add.

## The shape

```yaml
include:
  - id: "discover:go"
    inputs:
      - "go.mod"
      - kind: structural
        files: "**/*.go"
        lines: ["package ", "import "]
    outputs: [".giant/d/go.json"]
    command: "tools/discover-go.sh > .giant/d/go.json"
```

Two things to notice:

- It's under `include:`, not `targets:`. That's how Giant knows to run
  it during the bootstrap pass before the main build.
- Its `outputs:` is the JSON file the engine will merge.

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
  ]
}
```

Same target schema as `targets:` in `giant.yaml`. Giant merges these
into the build graph as if you'd typed them yourself.

## How the bootstrap pass works

1. **Config load.** Giant parses `giant.yaml`, sees the `include:`
   entry.
2. **Bootstrap build.** The discovery target runs through the normal
   build pipeline - its cache key includes its inputs, env, etc., and
   its output JSON is cached.
3. **Merge.** Giant reads each discovery output JSON, parses the
   target list, and adds them to the graph.
4. **Output-based dep inference.** With the full graph in hand, Giant
   walks input/output globs and infers dependencies.
5. **Main build.** Proceeds as normal.

On warm runs the discovery target itself cache-hits, the JSON is
restored from CAS, and the merge happens against a known graph in
milliseconds.

## Writing a discovery script

The script can be in any language; it just has to read files and write
JSON to stdout (or its declared output path). A typical Go discovery
script:

```bash
#!/usr/bin/env bash
set -euo pipefail

go list -json ./... | jq -s '
  { targets: map({
      id: ("go:pkg:" + .ImportPath),
      inputs: ([.Dir + "/**/*.go"] + .Deps),
      outputs: (if .Name == "main" then ["bin/" + .Name] else [] end),
      command: ("go build ./" + .ImportPath)
    })
  }
'
```

Whatever produces JSON works. Many discovery scripts are 20 lines of
shell + `jq`.

## What discovery DOESN'T do

- **It doesn't have access to the engine.** Discovery is a normal
  subprocess. It can't query the cache, can't see what other targets
  exist, can't invoke giant recursively.
- **It doesn't run during every build.** Like any target, it
  cache-hits if its inputs are unchanged. Editing function bodies in
  Go files won't trigger discovery to re-run if you used a structural
  input on `package`/`import` lines (which you should).

## Recursive discovery (waves)

A discovery target can emit more `include:` entries. The engine runs
discovery in **waves**: every `include:` in the current wave is built
in parallel, their outputs are parsed, any nested `include:` entries
form the next wave, and the cycle repeats until no new includes
appear.

```jsonc
// scripts/discover-top.sh writes this. wave 1.
{
  "include": [
    {
      "id": "discover:go",
      "inputs": ["scripts/discover-go.sh", "src/**/go.mod"],
      "outputs": [".giant/d/go.json"],
      "command": "scripts/discover-go.sh > .giant/d/go.json"
    },
    {
      "id": "discover:docker",
      "inputs": ["scripts/discover-docker.sh", "**/Dockerfile"],
      "outputs": [".giant/d/docker.json"],
      "command": "scripts/discover-docker.sh > .giant/d/docker.json"
    }
  ]
}
```

Wave 2 runs `discover:go` and `discover:docker` in parallel, picks up
their `targets:`, and if either of *those* emits more `include:`
entries those land in wave 3.

This makes a few patterns easy:

- **Sub-monorepos.** A top-level discovery enumerates owned
  sub-repos; each sub-repo's own discovery script generates its
  targets. The top-level config stays a one-liner.
- **Conditional layers.** A top-level discovery decides which deeper
  discoveries to run based on env vars, present directories, or
  feature flags - emit only the includes you actually need.
- **Plugins.** `giant-go-discovery`, `giant-rust-discovery`, etc.,
  contributed as independent scripts. A "register plugins" discovery
  emits an include per language found in the workspace.
- **Composable layers.** Each discovery is a small, independently
  testable script. Run any of them directly to see its output;
  compose via `include:` rather than language-specific module imports.

What to know:

- **Wave parallelism.** Inside a wave, everything runs in parallel.
  Between waves the engine has to finish the current wave's builds +
  parse their JSON before starting the next wave - not because later
  waves depend on earlier outputs (they usually don't) but because
  **the next wave's targets don't exist as graph nodes until the
  current wave's output has been merged**. Until then the engine
  literally doesn't know what to build next.
- **Caching.** Each wave's builds cache like any target. Warm bootstrap
  is still free, no matter how deep your discovery tree goes.
- **Cycle safety.** If discovery A emits an include for B, and B
  emits an include for A, the engine notices the duplicate target id
  and silently dedupes - it won't loop. The duplicate's been processed
  already, so there's nothing new to do.
- **Depth cap.** A hard limit of 32 waves catches pathological cases.
  Hit it and the build fails with a clear error pointing at the last
  wave's target ids.
- **Coupling between discoveries is just normal target deps.** A
  discovery script has no engine introspection - it's a subprocess
  with only its declared inputs. If discovery A needs to read what
  discovery B produces, declare B's output file as one of A's inputs.
  Output-based inference wires the edge, the executor schedules B
  before A. Same wave or different wave: doesn't matter, deps are
  honoured either way.

## Why discovery is a target

A few projects have asked "why not embed a scripting language for
this?" The short answer is in
[ADR-0001](https://github.com/johnae/giant/blob/main/docs/adr/0001-discovery-as-a-target.md).
The longer answer:

- **Caching falls out automatically.** Discovery is a target; targets
  are cached; warm bootstrap is free.
- **Language-agnostic.** Your discovery script can be bash, Python, Go,
  Rust, whatever you want. Giant doesn't care.
- **Debuggable in isolation.** Run the script directly to see what it
  produces - no Giant runtime in the loop.
- **Tiny core.** No embedded interpreter to ship, version, or sandbox.
