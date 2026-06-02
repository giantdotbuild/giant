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
│   ├── build.rs         # build/test → Command::Build on an in-process session
│   ├── test.rs
│   ├── session.rs       # the engine core: SessionState + the Command/Event loop
│   ├── watch.rs         # shared watch mechanics (excludes, debouncer) - not a subcommand
│   ├── affected.rs
│   ├── graph.rs
│   ├── explain.rs
│   ├── clean.rs
│   ├── external.rs      # porcelain dispatch (giant <name> → giant-<name>)
│   └── prep.rs          # shared "load config + return graph"
├── config.rs            # YAML/JSON parsing + static validation
├── model.rs             # core types: TargetSpec, CacheKey, ContentHash
├── graph.rs             # build graph + topological sort
├── selection.rs         # pattern language + affected detection
├── executor.rs          # parallel dispatch, cache key composition
├── cache.rs             # local content-addressed cache + LRU eviction
├── remote.rs            # Bazel HTTP cache (feature-gated)
├── watcher.rs           # notify-based file watcher
├── renderer.rs          # event-to-line renderer
├── events.rs            # NDJSON event types
├── git.rs               # repo discovery for --affected --base
├── paths.rs             # AbsPath / WsRelPath / OutputPath newtypes + mtime_ns helper
└── types.rs             # GlobPattern newtype
```

## The data flow of one build

```
giant build go:bin:server
  │
  ▼
[ cli/build.rs ]
  load config → prep::prepare → build graph
  │
  ▼
[ selection ] resolve_patterns(go:bin:server) → [go:bin:server]
  │
  ▼
[ cli/session.rs ] SessionState::handle_command(Command::Build{...})
  the same in-process engine giant session / giant tui drive; the CLI
  is just another protocol client. Events stream back to the tty
  renderer, which reads pass/fail off build.finished.
  │
  ▼
[ executor ] build(BuildJob)
  for each target in topo order:
    │
    ▼
  [ executor::compose_cache_key ]
    hash workspace + id + command + cwd + env + inputs + dep_outputs
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

## Output-based dep inference

Targets don't list each other by ID to express a dependency. Instead,
a target declares the outputs it produces, and the engine links any
target whose inputs match another target's outputs. That's how a
static target picks up a generated artifact without naming the target
that produced it. The graph builder runs this pass once after loading
`giant.yaml`'s `targets:`, before the topological sort.

Toolchain folding rides on the same pass: targets tagged `toolchain`
are folded into their dependents' cache keys so a toolchain change
invalidates everything built with it, without each target restating
the dependency.

## Watch loop

`--watch` is a flag on `build`/`test`, not a subcommand. It dispatches
`Command::WatchStart` to the same in-process `SessionState`; there is
one build-watch loop and it lives in the engine (`cli/session.rs`),
driven identically whether the client is the CLI or a TUI.

```
session: watch_loop(selection)
  build once
  spawn notify watcher → mpsc channel of changed paths
  loop:
    debouncer.next_batch()       # quiet=100ms, max=500ms
    affected_targets()           # intersect changed paths with selection
    emit watch.affected{ids}     # empty = change touched nothing selected
    if non-empty: build()
```

The debouncer is a `tokio::select!` between a sleep, the channel, and
a cancel token. The shared pieces - the exclude set, the debouncer,
and the per-cycle affected step - live in `cli/watch.rs`; the loop that
uses them is in `cli/session.rs`.

## NDJSON event protocol

Every part of the engine emits events through a `tokio::mpsc::Sender<Event>`.
The renderer task pulls from the matching `Receiver` and either prints
human-readable lines or serializes the raw event to NDJSON depending
on mode.

The same machinery backs every entry point. `giant session` fans the
events out to its stdin/stdout client; `giant build` / `giant test`
run the identical `Command::Build` through an in-process session and
feed the stream to the tty renderer. The renderer and the NDJSON
writer are two consumers of one engine dispatch - the CLI has no
private build path.

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
`spawn_blocking` wraps `std::fs` calls in `cache.rs`.

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
- Embedded scripting language.
- Plugin DLLs (porcelains via subprocess, not loaded code).

The pitch is a small, focused build engine. Anything that creeps the
surface beyond the engine gets pushed out as a porcelain.
