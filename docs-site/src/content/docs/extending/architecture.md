---
title: Architecture
description: The small core, the NDJSON protocol, and the porcelains around them.
---

Giant is built around one idea: **the core is a build engine you drive
over a protocol, and everything else is a porcelain.** Get that idea and
the rest of the system follows.

## The core does three things

The `giant` binary is small on purpose. It:

1. **Builds and caches.** It reads static `giant.yaml` files, builds a
   dependency graph, runs commands in parallel, and stores outputs in a
   content-addressed cache (local, and optionally remote).
2. **Speaks NDJSON.** Every build emits a stream of newline-delimited JSON
   events. `giant session` turns that into a two-way channel: JSON commands
   in on stdin, events out on stdout.
3. **Dispatches.** `giant <name>` runs a `giant-<name>` binary if `<name>`
   isn't built in.

That is the whole core. Its only built-in subcommands are **`session`**
(the engine over a pipe) and **`completions`** (shell completion for the
dispatcher). Look at `giant --help` and you'll see the rest -
`build`, `test`, `explain`, `graph`, `task` - but those are not in the
binary. They are separate `giant-*` programs the dispatcher found on your
PATH.

## Everything you type is a porcelain

A **porcelain** is a standalone binary named `giant-<name>`. When you run
`giant build`, the core doesn't have a build command - it looks for
`giant-build` (beside itself first, then on PATH) and execs it. The
first-party set:

| You type | Runs | What it is |
| --- | --- | --- |
| `giant build` / `test` / `verify` | `giant-build` | runs targets, renders progress |
| `giant explain` | `giant-explain` | why a target's cache key is what it is |
| `giant logs` | `giant-logs` | replay a target's captured output |
| `giant affected` | `giant-affected` | list targets a change touches |
| `giant clean` | `giant-clean` | prune the local cache |
| `giant graph` | `giant-graph` | print the dependency graph |
| `giant gen` | `giant-gen` | run config generators (offline) |
| `giant task` | `giant-task` | named commands with build deps |
| `giant tui` | `giant-tui` | interactive target browser |

Some of these link the engine as a Rust library (the build family runs in
process); others are pure [protocol clients](#two-ways-a-porcelain-talks-to-the-core)
that spawn a `session` and render what comes back (`explain` over
`query.explain`, `logs` over `logs.get`). From where you sit they are all
just `giant <name>`. The boundary is real but invisible.

An unknown name is an error:

```console
$ giant deploy
no such subcommand 'deploy': not a built-in and no 'giant-deploy' found
beside giant or on PATH.
hint: to run a task named 'deploy', use `giant task deploy`.
```

## Tasks are just a porcelain

Tasks are a good example of the model: named commands like `giant task fmt`
are not a core feature at all. They live entirely in the `giant-task`
porcelain.

- The core has no `task` subcommand and no `tasks:` schema. It never reads
  the `tasks:` block in your `giant.yaml`; `giant-task` does, with its own
  parser, and ignores everything else.
- Uninstall `giant-task` and the notion of a task is *gone*. `giant task`
  errors like any unknown name, and `tasks:` in your config is inert text
  the engine skips.
- Nothing in the core changes either way. It was never involved.

This is the design principle stated as a constraint: **new capability
arrives as a porcelain.** The TUI, the task runner, config generation, the
sandbox helper are opt-in software on your PATH; the engine carries none of
their weight. The core stays small enough to read in an afternoon because the
things that would grow it live outside it.

## Two ways a porcelain talks to the core

A porcelain that needs the engine has two transports, both the same
[NDJSON protocol](/reference/events/):

- **One-shot:** spawn `giant build --events ndjson`, read the event stream
  off stdout. Simple, stateless. Good for a status line or a CI summary.
- **Session:** spawn `giant session` once and speak commands on stdin
  while parsing events on stdout. The engine loads config once and stays
  warm. Good for a TUI, an IDE, or a web backend driving builds across many
  requests.

Because the protocol is the API, the client doesn't have to be a CLI at
all. A desktop app, a web service, or an editor extension can spawn
`giant session` and drive builds without linking a line of Giant's code.
See **[Controlling Giant](/guides/controlling-giant/)** for worked
examples.

## How one build flows

```
giant build //crates/giant:giant
  │  (dispatch: exec giant-build)
  ▼
[ giant-build ]  load config → scan + merge giant.yaml files → build graph
  resolve selection (//crates/giant:giant) → run through the engine adapter
  │  events stream back; the renderer prints them
  ▼
[ engine ]  for each target in topological order:
    compose cache key  = hash(command + cwd + env + file inputs + dep output hashes)
    │
    ├─ local cache hit?   → restore outputs from the CAS        (cache_hit)
    ├─ remote cache hit?  → pull blobs, write local entry        (remote_cache_hit)   [feature: remote]
    ├─ declared `exists:`? → already present, skip               (external_cache_hit)
    └─ miss → run the command, hash every output into the CAS,
              write the action-cache entry, upload it (remote)   (built)
```

The cache key is a SHA-256 over a deterministic byte stream - command, cwd,
environment, file-input hashes, and the output hashes of dependencies. It
does **not** include the workspace name or the target label, so the same
inputs hit the same entry across machines. See [The cache
key](/concepts/cache-key/) for the full story.

## The pieces

The engine is one Rust crate (a library plus the `giant` binary). The wire
protocol is a second small crate so porcelains can speak it without pulling
in the engine. The porcelains are their own crates.

```
crates/
├── giant/             the engine: config scan, graph, selection, executor,
│                      content-addressed cache, remote cache, file watcher,
│                      the session loop - plus the `giant` binary
│                      (session + completions + dispatch)
├── giant-protocol/    the wire types: Command, Event, TargetId, and a small
│                      client for spawning a session and collecting replies
├── giant-build/       build / test / verify
├── giant-explain/     explain          giant-logs/      logs
├── giant-affected/    affected         giant-clean/     clean
├── giant-graph/       graph            giant-gen/       config generators
├── giant-task/        the task runner  giant-tui/       the interactive UI
└── giant-sandbox/     the sandbox exec-wrapper helper
```

Inside `giant/`, the modules that matter:

- **`config`** - scan the tree for `giant.yaml`/`giant.json`, merge into one
  graph, resolve package-relative paths, validate (a bad field fails the
  load with a spanned error, never silently).
- **`graph`** - the build graph and its topological sort.
- **`selection`** - the pattern language (`//src/...`, `!exclusions`, tags)
  and affected detection, shared by every selection-taking porcelain.
- **`executor`** - parallel dispatch (a `tokio::JoinSet` bounded by CPU
  count) and cache-key composition.
- **`cache`** - the local content-addressed store (action cache + CAS) and
  LRU eviction.
- **`remote`** - the Bazel HTTP cache protocol, feature-gated.
- **`watcher`** - the `notify`-based file watcher behind `--watch` and the
  watch/affected subscriptions.
- **`session`** - the `SessionState` and the command/event loop that *is*
  the engine. The build family runs the identical loop in-process; the
  difference is only who reads the events.

## Generation is offline, and outside the engine

The engine reads static config. It never computes targets at build time -
no discovery pass, no matrix expansion, no language scanners. When a repo
has too many targets to hand-write, you **generate** the `giant.yaml`
files offline and check them in, exactly as you'd generate any other
source. `giant gen` runs your generators; the engine just reads the files
they wrote. See [Generating config](/guides/generating-config/).

## Why no daemon

A build tool that has to be running to be useful is one you have to
remember to start, and a daemon owns shared state (graph, cache index)
that needs locks, sync, and recovery. Giant skips all of it: every
invocation opens the cache directly, does its work, and exits. Watch mode
is the one long-lived loop, and it is just the engine running in one
process - when the process ends, the loop ends. No leftover state, nothing
to clean up.

If you *want* a warm engine - to avoid re-reading config per command -
that is exactly what `giant session` gives you, on demand, owned by
whatever started it.

## What is deliberately not here

- **Tasks** - `giant-task` (see above).
- **A TUI** - `giant-tui`. The core never takes over your terminal.
- **Service supervision** - use process-compose, overmind, or systemd-run.
- **An embedded scripting language** - generation runs offline; the engine
  reads the result.
- **Plugin DLLs** - porcelains are subprocesses over a protocol; nothing is
  loaded into the engine's address space. See [Why not plugin
  DLLs](/extending/porcelains/#why-not-plugin-dlls).
