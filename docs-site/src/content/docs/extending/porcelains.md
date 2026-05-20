---
title: Porcelains
description: Extend Giant via standalone binaries on PATH.
---

A **porcelain** is an optional binary that extends Giant. The model is
the same one git, cargo, kubectl, and jj use: run `giant <name>`, and
if `<name>` isn't a built-in subcommand, Giant looks for `giant-<name>`
on PATH and execs it.

The core stays small. Anything that doesn't belong in the engine -
tasks, a TUI, custom deploy pipelines - ships as a separate binary
that users install if they want it.

## How dispatch works

```console
$ giant build              # built-in
$ giant test               # built-in
$ giant task deploy        # not built-in → execs giant-task with [deploy]
$ giant nope               # error: no such subcommand, no giant-nope on PATH
```

On Unix the dispatch uses `exec(3)` so the porcelain replaces the
giant process. Signals (Ctrl-C, SIGTERM) go directly to the porcelain;
there's no parent in the middle to translate them. On non-Unix Giant
spawns and waits, propagating the exit code.

## Writing a porcelain

Any executable named `giant-<name>` on PATH works. Bash, Python, Go,
Rust - whatever. Two responsibilities:

1. **Read your own CLI args.** `argv[1..]` is whatever the user passed
   after `<name>`.
2. **(Optional) Talk to giant via NDJSON.** Spawn `giant build --events
   ndjson` and consume its stdout, or read events from
   `giant serve`'s Unix socket when it lands.

Minimum viable porcelain (bash):

```bash
#!/usr/bin/env bash
# /usr/local/bin/giant-hello
set -euo pipefail
echo "hello from giant-$0 - you passed: $*"
```

```console
$ giant hello world
hello from giant-/usr/local/bin/giant-hello - you passed: world
```

## A porcelain that wraps `giant build`

```bash
#!/usr/bin/env bash
# /usr/local/bin/giant-status
# Runs a quiet affected build and prints a one-line summary.
set -euo pipefail

base="${1:-main}"
giant build --affected --base "$base" --events ndjson \
  | jq -r 'select(.t == "build.finished") |
           "\(.counts.built) built, \(.counts.cache_hit) cached, \(.counts.failed) failed in \(.duration_ms)ms"'
```

```console
$ giant status main
3 built, 12 cached, 0 failed in 1240ms
```

## Naming

Use lowercase, hyphen-separated names. `giant-task`, `giant-tui`,
`giant-deploy`. The dispatch is case-sensitive on case-sensitive
filesystems.

Reserved built-in names (don't shadow these): `build`, `test`,
`watch`, `affected`, `graph`, `clean`, `explain`.

## Communicating with the engine

Three transport options, all speaking the same [NDJSON
protocol](/reference/events/):

### 1. Subprocess + stdout (the simple case)

Your porcelain spawns `giant build --events ndjson` and consumes
stdout. One-shot, easy, no shared state.

### 2. Subprocess + stdin/stdout (planned)

For interactive porcelains (a TUI controlling a watch session), giant
will accept commands on stdin alongside emitting events on stdout.

### 3. Unix socket via `giant serve` (planned)

Multi-client scenarios: a TUI + a CI tail + your IDE all attached to
one engine instance. `giant serve` runs a Unix socket; clients connect
and speak the same protocol.

## Distribution

Porcelains live in their own repos. Users install them however they
install other CLIs - `cargo install`, `brew`, `apt`, drop a binary in
`~/.local/bin`. The dispatch shim has no opinion about how they got
there; it just looks at PATH.

If you ship one we'd love to know about it. Open a PR adding it to
the "Community porcelains" section here.

## Why not plugin DLLs

Three reasons:

- **ABI versioning** is hard. Rust doesn't have a stable ABI; we'd
  ship a C ABI just for plugin compat.
- **Security surface.** Loaded code runs in our process.
- **Platform-specific dynamic loading.** dlopen/Windows DLLs/macOS
  dylibs have different semantics and quirks.

Subprocess-based porcelains are strictly simpler. The Unix socket
transport plus the well-defined NDJSON protocol give you everything
loadable plugins would, without the headaches.
