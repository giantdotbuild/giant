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

The engine is language-agnostic. Targets are `inputs → command → outputs`. Go
packages, Docker images, protobuf - all of them are just targets with the right
inputs, command, and outputs, declared in `giant.yaml`.

> Full docs and a quickstart at **[giant.build](https://giant.build)**.

## Install

From source:

```bash
git clone https://github.com/johnae/giant
cd giant
cargo install --path crates/giant
```

The binary is called `giant`. With the `remote` feature flag it also speaks the
Bazel HTTP cache protocol; without it the executable stays smaller and offline-only.

```bash
cargo install --path crates/giant --features remote
```

## A first config

`giant.yaml` in your workspace root:

```yaml
workspace:
  name: hello-giant
cache:
  dir: ~/.cache/giant

targets:
  - name: "greet"
    inputs: ["name.txt"]
    outputs: ["greeting.txt"]
    command: "echo \"hello, $(cat name.txt)\" > greeting.txt"
```

A target's identity is its label, derived from where its `giant.yaml`
lives: `//<package>:<name>`. A target in the root file has the empty
package, so this one is `//:greet`.

```bash
$ echo world > name.txt
$ giant build
✓ BUILD   //:greet   4ms
  OK    1 built · 0 cached · 0 failed  in 4ms

$ giant build
✓ CACHE   //:greet   1ms
  OK    0 built · 1 cached · 0 failed  in 1ms
```

The second run hits the cache. Edit `name.txt` and the target rebuilds.

## Selecting targets

Targets are selected by label, with git/cargo-style patterns:

```bash
giant build //src/go/server:server       # exact label
giant build '//src/go:*'                  # every target in one package
giant build '//src/go/...'                # a package and everything under it
giant build '//...' '!//src/legacy/...'   # whole tree, then exclude
giant build --tag release --no-tag flaky
giant build --affected --base main        # only what changed since main
```

`:*` selects a package, `/...` recurses, `//...` is everything, `!pattern`
excludes. An exact-label typo errors; a glob that matches nothing is silent.
The same language is used by `giant test`, `giant build --watch`, and
`giant affected`.

## Common commands

```bash
giant build             # build all non-test targets
giant test              # run all test targets
giant build --watch     # initial build, then rebuild on file changes
giant affected --base main    # list what would rebuild, no work done
giant graph             # show the dependency graph
giant explain //src/go/server:server   # explain a target's cache key
giant clean             # clear the local cache
```

`--quiet`/`-q` on `build` and `test` reduces output to failures plus
the summary. `--events ndjson` switches the output to a machine-readable
event stream consumed by porcelains.

## Porcelains

Unknown subcommands dispatch to `giant-<name>` on PATH, the git/cargo/kubectl
model. Run `giant task deploy` and it execs `giant-task deploy` if present.
The wire format between core and porcelains is the NDJSON event/command
protocol. Several first-party porcelains ship (build/test, tasks, tui,
generation, logs, explain, graph, affected, clean), dispatched git-style;
the same shim picks up any community-built ones on PATH.

## Remote cache

With `--features remote`:

```yaml
remote:
  enabled: true
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
  enabled: true               # default off; without this the remote cache is a no-op
  url: "https://..."
  auth: { kind: bearer, token_env: TOKEN }

targets:
  - name: "<unique-in-package>"  # label is //<package>:<name> (//:name in the root file)
    inputs: [...]             # globs, package-relative; // anchors the workspace root
    outputs: [...]            # package-relative (// for root-level)
    deps: [...]               # explicit deps as labels //pkg:name (generation fills the rest)
    command: "..."
    cwd: "..."                # package-relative; default = the package dir
    env: { KEY: VAL }
    test: false
    tags: [release, linux]
    cache: true               # set false to never cache
    remote_cache: true
    exists: "..."             # external check; if it succeeds, command is skipped
    timeout_secs: 300         # seconds
```

## How it works

A short tour of what's where:

- `crates/giant/src/executor.rs` - parallel dispatch, cache key composition,
  early-cutoff, remote-cache fallback chain.
- `crates/giant/src/cache.rs` - local content-addressed cache; LRU eviction.
- `crates/giant/src/graph.rs` - dependency graph (explicit `deps`, output
  uniqueness check).
- `crates/giant/src/selection.rs` - pattern language (labels, exclusions, tags,
  test mode).
- `crates/giant/src/cli/` - the `session` and `completions` built-ins plus
  porcelain dispatch.
- `crates/giant-build/` - the `build`/`test`/`verify` porcelain and the
  line-streaming + NDJSON renderer.
- `crates/giant-task/` - task-runner porcelain ([docs](docs-site/src/content/docs/extending/giant-task.md)).

The design is described in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Dogfood

Giant uses its own `giant.yaml` for everything in this repo that isn't
the cargo build of the engine itself.

**Bootstrap once:**

```bash
cargo install --path crates/giant   # gives you a `giant` on PATH
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

Working: build, test, `--watch`, affected, graph, explain, clean, porcelain
dispatch, local + remote cache, NDJSON event stream, LRU cache eviction, and
opt-in sandboxing with `giant verify` (a sandboxed audit in a disposable
worktree). `giant session` runs a persistent engine that live-reloads on
`giant.yaml` edits, and the command channel lets porcelains send commands back
over the protocol. `giant-task` ships in `crates/giant-task/` and handles
tasks, services with readiness probes, needs/finally, args, and shell
completions across six shells. `giant-tui` ships in `crates/giant-tui/` - a
full TUI with a tag/status-toggle surface for filtering the build.

Not yet built: the `giant-web` browser bridge and remote execution.

## License

Apache-2.0. See [LICENSE](LICENSE).
