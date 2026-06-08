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
$ giant session            # built-in (the engine over a pipe)
$ giant build              # not built-in → execs giant-build
$ giant task deploy        # not built-in → execs giant-task with [deploy]
$ giant nope               # error: no such subcommand, no giant-nope found
```

Only `session` and `completions` are built into the core binary.
Everything else - including `build`, `test`, and `explain` - is a
`giant-*` program the dispatcher finds and execs. An unknown name is an
error, with a hint to try `giant task <name>`.

Giant looks for `giant-<name>` **beside its own binary first** (the suite
installs its porcelains in one directory), then on PATH. On Unix the
dispatch uses `exec(3)` so the porcelain replaces the giant process -
signals (Ctrl-C, SIGTERM) go straight to it, no parent in the middle. On
non-Unix Giant spawns and waits, propagating the exit code.

## Writing a porcelain

Any executable named `giant-<name>` on PATH works. Bash, Python, Go,
Rust - whatever. Two responsibilities:

1. **Read your own CLI args.** `argv[1..]` is whatever the user passed
   after `<name>`.
2. **(Optional) Talk to giant via NDJSON.** Spawn `giant build --events
   ndjson` and consume its stdout, or spawn `giant session` for a
   persistent engine with a bidirectional command channel.

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

Only two names are reserved (the core's built-ins): `session` and
`completions`. Everything else is fair game - though if you name yours
`giant-build` you'll shadow the first-party build porcelain, so pick
something distinct.

## First-party porcelains

The Giant suite ships these. Each is an ordinary `giant-*` binary; install
the ones you want, skip the rest.

| Command | Binary | Does |
| --- | --- | --- |
| `giant build` / `test` / `verify` | `giant-build` | run targets, render progress |
| `giant explain` | `giant-explain` | what feeds a target's cache key |
| `giant logs` | `giant-logs` | replay a target's last captured output |
| `giant affected` | `giant-affected` | list targets a change touches |
| `giant clean` | `giant-clean` | prune the local cache |
| `giant graph` | `giant-graph` | print the dependency graph |
| `giant gen` | `giant-gen` | run config [generators](/guides/generating-config/) |
| `giant task` | `giant-task` | named commands - see [giant-task](/extending/giant-task/) |
| `giant tui` | `giant-tui` | interactive browser - see [giant-tui](/extending/giant-tui/) |

`giant-explain` and `giant-logs` are pure protocol clients: they spawn a
`giant session`, send one read query, and render the reply. They're the
worked example for the [Controlling Giant](/guides/controlling-giant/)
guide: thin clients that render what the engine reports, with no build logic
of their own.

## Communicating with the engine

A porcelain that needs the engine talks to it over the [NDJSON
protocol](/reference/events/), the same interface the CLI uses - either
one-shot (`giant build --events ndjson`, read stdout) or a warm
[`giant session`](/reference/cli/#giant-session) with a two-way command
channel. **[Controlling Giant](/guides/controlling-giant/)** walks through
both with runnable Node and Python clients. A porcelain is just such a
client that happens to be named `giant-<name>`.

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

Subprocess-based porcelains are strictly simpler. The well-defined
NDJSON protocol over stdin/stdout gives you everything loadable
plugins would, without the headaches.
