A build orchestration tool with content-addressed caching for monorepos. Tracks
dependencies across language boundaries, runs builds in parallel, and caches
outputs locally (and optionally to a shared HTTP cache).

```console
$ giant build
✓ BUILD   proto:api              340ms
✓ CACHE   go:lib:auth              2ms
↓ REMOTE  rust:lib:core          120ms
✓ BUILD   go:bin:server          1.24s

  OK    1 built · 2 cached · 1 remote  in 1.27s
```

The engine is language-agnostic. Targets are `inputs → command → outputs`. Anything
beyond that - Go packages, Docker images, protobuf - comes from discovery scripts
the engine runs to materialize targets at config time.

> Full docs and a quickstart at **[giant.build](https://giant.build)**.

## Install

From source:

```bash
git clone https://github.com/johnae/giant
cd giant
cargo install --path .
```

The binary is called `giant`. With the `remote` feature flag it also speaks the
Bazel HTTP cache protocol; without it the executable stays smaller and offline-only.

```bash
cargo install --path . --features remote
```

## A first config

`giant.yaml` in your workspace root:

```yaml
workspace:
  name: hello-giant
cache:
  dir: ~/.cache/giant

targets:
  - id: "demo:greet"
    inputs: ["name.txt"]
    outputs: ["greeting.txt"]
    command: "echo \"hello, $(cat name.txt)\" > greeting.txt"
```

```bash
$ echo world > name.txt
$ giant build
✓ BUILD   demo:greet   4ms
  OK    1 built · 0 cached · 0 failed  in 4ms

$ giant build
✓ CACHE   demo:greet   1ms
  OK    0 built · 1 cached · 0 failed  in 1ms
```

The second run hits the cache. Edit `name.txt` and the target rebuilds.

## Selecting targets

Patterns work like git/cargo:

```bash
giant build go:bin:server               # exact id
giant build 'go:bin:*'                  # one segment with *
giant build 'go:**' '!go:test:*'        # union, then exclude
giant build --tag release --no-tag flaky
giant build --affected --base main      # only what changed since main
```

`*` stops at `:`; `**` crosses. `!pattern` excludes. Exact-id typos error;
glob misses go silent. The same language is used by `giant test`, `giant watch`,
and `giant affected`.

## Common commands

```bash
giant build             # build all non-test targets
giant test              # run all test targets
giant watch             # initial build, then rebuild on file changes
giant affected --base main    # list what would rebuild, no work done
giant graph             # show the dependency graph
giant explain go:bin:server   # explain a target's cache key
giant clean             # clear the local cache
```

`--quiet`/`-q` on `build`, `test`, `watch` reduces output to failures plus
the summary. `--events ndjson` switches the output to a machine-readable
event stream consumed by porcelains.

## Discovery

Some targets are too dynamic to hand-write - every Go package, every Dockerfile,
every Rust crate. Discovery targets emit JSON that giant merges into the build
graph:

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

`tools/discover-go.sh` writes something like:

```json
{ "targets": [
  { "id": "go:pkg:internal/auth",
    "inputs": ["internal/auth/**/*.go"],
    "outputs": ["bin/auth"],
    "command": "go build -o bin/auth ./internal/auth" }
]}
```

Giant runs discovery before the main build, merges the emitted targets, and
infers cross-target dependencies wherever one target's output matches another's
input. The structural input above only re-runs discovery when `package`/`import`
lines change - function-body edits keep the cache warm.

## Porcelains

Unknown subcommands dispatch to `giant-<name>` on PATH, the git/cargo/kubectl
model. Run `giant task deploy` and it execs `giant-task deploy` if present.
The wire format between core and porcelains is the NDJSON event/command
protocol (see `docs/tdd/0004-event-protocol.md`). No porcelains ship yet -
the dispatch shim is there for community-built ones.

## Remote cache

With `--features remote`:

```yaml
remote:
  url: "https://cache.example.com"
  auth: { kind: bearer, token_env: CACHE_TOKEN }
```

Speaks the Bazel HTTP cache protocol - works against bazel-remote, BuildBuddy,
sccache, and S3-backed caches via the same protocol.

## Configuration reference

The full `giant.yaml` shape:

```yaml
workspace:
  name: <required>

cache:
  dir: ~/.cache/giant
  max_size_gb: 20             # 0 = unlimited; eviction is disabled
  evict_when_above_pct: 100   # trigger
  evict_target_pct: 80        # evict down to this

remote:                       # feature-gated
  url: "https://..."
  auth: { kind: bearer, token_env: TOKEN }

include:                      # discovery targets, run before main build
  - id: "..."
    inputs: [...]
    outputs: [...]
    command: "..."

targets:
  - id: "<unique-id>"
    inputs: [...]             # globs, relative to workspace root
    outputs: [...]            # relative to the target's cwd
    deps: [...]               # additional explicit deps (most are inferred)
    command: "..."
    cwd: "..."                # workspace-relative; default = root
    env: { KEY: VAL }
    test: false
    tags: [release, linux]
    cache: true               # set false to never cache
    remote_cache: true
    exists: "..."             # external check; if it succeeds, command is skipped
    timeout: 300              # seconds
```

## How it works

A short tour of what's where:

- `src/executor.rs` - parallel dispatch, cache key composition, early-cutoff,
  remote-cache fallback chain.
- `src/cache.rs` - local content-addressed cache; LRU eviction.
- `src/structural.rs` - three-stage structural input fingerprinting
  (cold filesystem walk → mtime-skip warm validation → git fast-path).
- `src/discovery.rs` - discovery target bootstrap and merge.
- `src/graph.rs` - dependency graph, output-based dep inference.
- `src/selection.rs` - pattern language (globs, exclusions, tags, test mode).
- `src/renderer.rs` - colored line-streaming output + NDJSON pass-through.
- `src/cli/` - subcommand handlers.

Design docs are in `docs/adr/` (decisions) and `docs/tdd/` (technical specs).

## Dogfood

Giant uses its own `giant.yaml` for everything in this repo that isn't
the cargo build of the engine itself.

**Bootstrap once:**

```bash
cargo install --path .        # gives you a `giant` on PATH
giant task bin                # builds bin/giant + bin/giant-task
```

The `bin/` directory is on PATH inside the devenv shell (via
`enterShell`), so once `giant task bin` runs once, the freshly-built
binaries replace whatever the devenv shell would otherwise pick. From
then on giant builds itself - the next `giant task bin` runs the
just-built giant, which rebuilds itself if sources changed and copies
the new binary back into `bin/`. Unix is happy to replace a running
binary; the in-flight process keeps the old inode.

**Day-to-day:**

```bash
giant task list             # see every command this repo defines
giant task fmt              # cargo fmt --all
giant task check            # fmt-check + clippy + test-all
giant task docs             # builds the static docs site
giant task docs-dev         # serves the docs site at :4321
giant task release          # check + release-build + docs
giant task bin              # refresh bin/giant + bin/giant-task
giant build docs:build      # the docs-site cache layer (npm install + astro build)
```

`giant build docs:build` is the interesting one - npm install + astro
build take ~5 s cold and 0 ms warm, because giant caches the directory
contents.

## Status

Working: build, test, watch, affected, graph, explain, clean, porcelain dispatch,
local + remote cache, discovery, structural inputs with git fast-path, NDJSON event
stream, LRU cache eviction. `giant-task` ships in `crates/giant-task/` and handles
tasks, services with readiness probes, needs/finally, args, shell completions
across six shells.

Not yet built: command channel for porcelains to send commands back to the engine
(`giant serve`), tags-as-toggleable surface in a TUI, the `giant-tui` porcelain
itself.

## License

MIT.
