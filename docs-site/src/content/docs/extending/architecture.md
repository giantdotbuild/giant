---
title: Architecture
description: A tour of the engine internals.
---

Giant's whole engine is a small Rust crate - small enough to read in
an afternoon, and meant to stay that way. This page is the map.

## File layout

```
src/
├── main.rs              # entry point; sets up tokio, dispatches to cli::run
├── lib.rs               # crate root; re-exports the high-traffic types
├── cli/                 # subcommand handlers
│   ├── mod.rs
│   ├── build.rs
│   ├── test.rs
│   ├── watch.rs
│   ├── affected.rs
│   ├── graph.rs
│   ├── explain.rs
│   ├── clean.rs
│   ├── external.rs      # porcelain dispatch (giant <name> → giant-<name>)
│   └── prep.rs          # shared "load config + bootstrap + return graph"
├── config.rs            # YAML/JSON parsing + static validation
├── model.rs             # core types: TargetSpec, CacheKey, ContentHash
├── graph.rs             # build graph + topological sort
├── discovery.rs         # bootstrap + JSON merge
├── selection.rs         # pattern language + affected detection
├── executor.rs          # parallel dispatch, cache key composition
├── cache.rs             # local content-addressed cache + LRU eviction
├── remote.rs            # Bazel HTTP cache (feature-gated)
├── structural.rs        # 3-stage structural input fingerprinting
├── watcher.rs           # notify-based file watcher
├── renderer.rs          # event-to-line renderer
├── events.rs            # NDJSON event types
├── git.rs               # affected_files_since for --affected --base
├── paths.rs             # AbsPath / WsRelPath / OutputPath newtypes
└── types.rs             # GlobPattern newtype
```

## The data flow of one build

```
giant build go:bin:server
  │
  ▼
[ cli/build.rs ]
  load config → prep::prepare → discovery bootstrap → merge graph
  │
  ▼
[ selection ] resolve_patterns(go:bin:server) → [go:bin:server]
  │
  ▼
[ executor ] build(BuildJob)
  for each target in topo order:
    │
    ▼
  [ executor::compose_cache_key ]
    hash workspace + id + command + cwd + env + inputs + structural + dep_outputs
    │
    ▼
  [ cache::get_ac(key) ]
    hit?  → restore outputs from CAS → emit target.finished{cache_hit}
    miss? ↓
    │
    ▼
  [ remote::get_ac(key) ]   (if --features remote)
    hit?  → pull CAS blobs → write local AC → emit target.finished{remote_cache_hit}
    miss? ↓
    │
    ▼
  [ exists? ]
    yes?  → emit target.finished{external_cache_hit}
    no?   ↓
    │
    ▼
  [ executor::run_command ]
    spawn command via shell
    capture stdout/stderr (stream to renderer as target.log events)
    │
    ▼
  [ executor::fingerprint_outputs ]
    hash every output file → put bytes in CAS → write AC entry
    upload to remote cache in background (if --features remote)
    │
    ▼
  emit target.finished{built}
```

## The cache key

A SHA-256 over a deterministic byte stream. The composition is in
`executor::compose_cache_key`. See [The cache key](/concepts/cache-key/)
for the user-facing story; the source is the source of truth for the
exact bytes.

## Discovery bootstrap

`cli::prep::prepare` runs every `include:` target through the normal
build pipeline (it gets its own `BuildJob` with `build_id` like
`bootstrap_<hash>`). After each succeeds, its output JSON is parsed
and merged into the graph. Then output-based dep inference runs over
the merged graph.

In the renderer, events with a `bootstrap_*` build id are filtered out
of human output - the user sees one summary per real build, not two.
Failures still surface.

## Structural inputs

Three stages, all in `structural.rs`:

1. **Cold compute** (`compute_fingerprint_cold`) - walk the
   filesystem, read every matched file, hash the prefix-matching lines,
   write a sidecar.
2. **Mtime-skip warm** (`compute_fingerprint_warm`) - walk again, but
   for each file compare `(mtime, size)` against the sidecar; reuse
   the recorded hash if unchanged.
3. **Git fast-path** (`compute_fingerprint_via_git`) - when in a git
   repo, ask `git status` for the modified file list; only revalidate
   those, accept the rest from the sidecar.

Stage 3 lets a 10k-file workspace answer a warm structural query in a
few milliseconds.

## Watch loop

```
spawn notify watcher → mpsc channel of changed paths
loop:
  debouncer.next_batch()       # quiet=100ms, max=500ms
  re-run prep::prepare         # bootstrap may emit new targets
  resolve_patterns()           # user's selection
  affected_targets()           # intersect with file changes
  if non-empty: build()
```

The debouncer is a `tokio::select!` between a sleep, the channel, and
a cancel token. Source in `cli/watch.rs`.

## NDJSON event protocol

Every part of the engine emits events through a `tokio::mpsc::Sender<Event>`.
The renderer task pulls from the matching `Receiver` and either prints
human-readable lines or serializes the raw event to NDJSON depending
on mode.

The same machinery backs `giant session` - events fan out to the
attached stdin/stdout client with the same shape, no serialization
differences.

## Tokio task layout

- **Main task**: runs `cli::run`, awaits the renderer task at the end.
- **Renderer task**: consumes events from the mpsc, writes lines to
  stdout. One per build.
- **Executor**: uses `tokio::JoinSet` to spawn per-target tasks.
  Bounded by `parallelism` (default = num CPUs). Each task does its
  own cache lookup + (if needed) command execution.
- **Remote upload task** (feature-gated): one background task that
  drains an mpsc of "upload this AC entry + its blobs" requests.
- **Watcher task**: the `notify` callback writes to an mpsc that the
  watch loop reads.

All tasks are async. No synchronous file I/O on the runtime -
`spawn_blocking` wraps `std::fs` calls in `cache.rs` and `structural.rs`.

## Why no daemon

Two reasons. **Cost:** a build tool that needs to be running to be
useful is a build tool you have to remember to start. **State:** a
daemon owns shared state (graph, cache index) that needs sync, locks,
and recovery semantics. Without a daemon, every `giant` invocation
opens the cache directly, reads what it needs, exits.

Watch mode is the exception - it's the same engine in a loop, in one
process. When the process ends, the loop ends. No leftover daemon to
clean up.

## What's NOT in the engine

- Tasks (`giant-task` porcelain in `crates/giant-task/`; see
  [its docs page](/extending/giant-task/)).
- TUI (`giant-tui` porcelain in `crates/giant-tui/`; see
  [its docs page](/extending/giant-tui/)).
- Service supervision (process-compose / overmind / systemd-run).
- Embedded scripting language (discovery is a target, not a script
  embedded in the engine).
- Plugin DLLs (porcelains via subprocess, not loaded code).

The pitch is a small, focused build engine. Anything that creeps the
surface beyond the engine gets pushed out as a porcelain.
