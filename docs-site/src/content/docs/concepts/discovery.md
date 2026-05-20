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
- **It can't emit other discovery targets.** Discovery is one level
  deep. (If you need that, write a script that produces the right
  static `include:` entries upstream - a tiny "build the build-tool's
  config" target.)
- **It doesn't run during every build.** Like any target, it
  cache-hits if its inputs are unchanged. Editing function bodies in
  Go files won't trigger discovery to re-run if you used a structural
  input on `package`/`import` lines (which you should).

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
