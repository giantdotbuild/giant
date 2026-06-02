---
title: Event protocol (NDJSON)
description: Machine-readable event stream for porcelains and IDE plugins.
---

Every Giant build can emit a stream of newline-delimited JSON events
on stdout. Each event is one line, one JSON object, terminated by `\n`.
The protocol is the contract between core and any porcelain (a TUI,
an IDE plugin, your CI dashboard).

## Enabling

```bash
giant build --events ndjson
```

When `--events ndjson` is set, the human-readable renderer is replaced
with a per-event JSON serializer. Stderr is unaffected.

## Envelope

```jsonc
{ "t": "<event type>",
  "...": "type-specific fields"
}
```

The `t` field discriminates. All other fields are type-specific.

## Event catalogue

### Engine lifecycle

```jsonc
{ "t": "engine.hello",
  "version": "0.1.0",
  "protocol": 1,
  "workspace": "/home/me/project" }

{ "t": "engine.shutdown",
  "reason": "graceful",     // or "signal", "error"
  "error": "..."            // present only on error
}
```

After `engine.hello`, a persistent session streams its catalog - one
`target.described` per target in the graph - then `engine.ready`
to signal it will accept commands.

```jsonc
{ "t": "target.described",
  "id": "//src/go/server:server",
  "command": "go build -o bin/server ./cmd/server",
  "tags": ["go"],          // omitted when empty
  "test": false,           // omitted when false
  "inputs": ["cmd/server/**/*.go"],
  "outputs": ["//bin/server"],
  "deps": ["//proto:gen"] }

{ "t": "engine.ready" }
```

### Config

```jsonc
{ "t": "config.loaded",
  "workspace_name": "myproj",
  "target_count": 42 }

{ "t": "config.error",
  "file": "giant.yaml",
  "line": 14,
  "column": 5,
  "message": "unknown field `inptus`" }
```

### Catalog (live reload)

A running session watches `giant.yaml` / `giant.json`. On an edit (or an
explicit `config.reload` command) it re-reads the config and re-emits the
catalog without a restart. The swap is bracketed by these two events;
between them the client drops its old catalog and rebuilds it from the
fresh `target.described` stream.

```jsonc
{ "t": "catalog.invalidating" }
{ "t": "catalog.ready" }
```

Between the two, the engine re-streams `target.described` for the new
graph. On a reload that fails to parse, the engine keeps the old
catalog and emits `command.error` instead of `catalog.ready`.

### Build lifecycle

```jsonc
{ "t": "build.started",
  "id": "b_4f9c",
  "selection": ["//src/go/..."],
  "target_ids": ["//src/go/server:server", "//src/go/client:client"],
  "parallelism": 16 }

{ "t": "build.finished",
  "id": "b_4f9c",
  "ok": true,
  "duration_ms": 1240,
  "counts": { "built": 2, "cache_hit": 1, "failed": 0, "skipped": 0 } }
```

### Target lifecycle

```jsonc
{ "t": "target.queued",
  "build": "b_4f9c",
  "id": "//src/go/server:server",
  "deps": ["//proto:gen"] }

{ "t": "target.started",
  "build": "b_4f9c",
  "id": "//src/go/server:server",
  "cache_key": "3a7f9c...",
  "command": "go build -o bin/server ./cmd/server" }

{ "t": "target.log",
  "build": "b_4f9c",
  "id": "//src/go/server:server",
  "stream": "stdout",       // or "stderr"
  "line": "go: downloading github.com/...",
  "truncated": false }      // present and true if the line was cut short

{ "t": "target.finished",
  "build": "b_4f9c",
  "id": "//src/go/server:server",
  "result": "built",        // built | cache_hit | remote_cache_hit | external_cache_hit | skipped | failed
  "duration_ms": 1240,
  "exit_code": 0,
  "outputs": ["bin/server"],
  "error": null }
```

### Watch

```jsonc
{ "t": "watch.started", "filter": "//src/go/..." }
{ "t": "watch.batch",
  "paths": ["src/main.go"],
  "more": 0,
  "config_changed": false }
{ "t": "watch.affected", "target_ids": ["//src/go/server:server"] }
{ "t": "watch.state", "state": "building" }    // idle | building | building_with_pending | reloading_config | config_error
{ "t": "watch.stopped" }
```

`watch.affected` fires once per change cycle; an empty `target_ids`
means a real change touched nothing in the selection (no build runs).

### Watch subscription (notify-only)

In response to `watch.subscribe`, the engine pins a file watcher and
notifies the client whenever any input of the subscribed targets - or
their transitive deps - changes. No build runs; the client decides what
to do. This is what backs dep-aware task watching in `giant-task`.

```jsonc
{ "t": "watch.changed",
  "paths": ["proto/user.proto"] }   // advisory; the signal is the event itself
```

`watch.unsubscribe` ends it.

### Affected subscription

Fired in response to `affected.subscribe`. The first event is the
initial snapshot; the engine then keeps a file watcher pinned and
re-emits whenever the affected set changes.

```jsonc
{ "t": "affected.changed",
  "base": "main",
  "target_ids": ["//src/go/server:server", "//crates/core:core"] }

{ "t": "affected.error",
  "base": "origin/missing",
  "message": "git diff against origin/missing: unknown revision" }
```

The subscription is single-shot per session - a second
`affected.subscribe` replaces the first, and `affected.unsubscribe`
ends it.

### Backpressure

If the consumer is slow, Giant drops log events first (build lifecycle
events are never dropped). After a drop, you'll see:

```jsonc
{ "t": "protocol.dropped",
  "count": 4837,
  "build": "b_4f9c",
  "target": "//src/go/server:server" }
```

## Versioning

`engine.hello.protocol` is bumped on breaking changes. Clients should
check it at startup and refuse to talk to incompatible versions.
Current: `1`.

Additive changes (new event types, new optional fields) don't bump the
version. Clients should tolerate unknown event types by skipping them.

## Command channel

`giant session` accepts NDJSON commands on stdin and emits events on
stdout - same wire format, same `t`/`c` envelope discipline. The TUI
and any other porcelain drive the engine through this channel.

Start a session:

```bash
giant session --events ndjson <commands.jsonl >events.jsonl
```

Or attach interactively from another tool - write commands to the
child's stdin, parse events from its stdout.

Command shapes (full list in `crates/giant/src/commands.rs`):

```jsonc
{ "c": "build",
  "command_id": "c_1",
  "targets": ["//src/go/server:server"],
  "fresh": false }

{ "c": "watch.start",
  "command_id": "c_2",
  "targets": ["//src/go/server:server"] }

{ "c": "watch.stop",
  "command_id": "c_3" }

{ "c": "cancel",
  "command_id": "c_4",
  "build": "b_4f9c" }

{ "c": "affected.subscribe",
  "command_id": "c_5",
  "base": "main" }

{ "c": "affected.unsubscribe",
  "command_id": "c_6" }

{ "c": "watch.subscribe",
  "command_id": "c_7",
  "targets": ["//proto:gen"],   // watch these + their transitive deps
  "globs": ["proto/**/*.proto"] }   // plus any matching path

{ "c": "watch.unsubscribe",
  "command_id": "c_8" }

{ "c": "config.reload",
  "command_id": "c_9" }

{ "c": "shutdown",
  "command_id": "c_10" }
```

`watch.subscribe` is notify-only - the engine replies with
`watch.changed` events, never builds. `config.reload` forces a
config re-read + catalog re-emit (the same thing a `giant.yaml` edit
triggers automatically).

Each command is acknowledged with a `command.accepted` (carrying the
allocated `build_id` if applicable) or `command.rejected` (with a
reason - e.g. `"watch is active - send watch.stop first"`). A command
that is accepted but then fails mid-flight (e.g. config can't be parsed on
a reload) emits `command.error { command_id, message }` - distinct from
`build.finished { ok: false }`, which is a targeted build failure.
Subsequent events for the work the command kicked off (`build.*`,
`target.*`) come back on the normal event stream.

Session lifecycle: on `engine.hello` plus the initial
`target.described` catalog stream plus `engine.ready`, the session is
ready to accept commands. Closing stdin drains in-flight work and
exits cleanly.

## Consuming the stream

The simplest consumer is `jq`:

```bash
giant build --events ndjson \
  | jq -c 'select(.t == "target.finished") | {id, result, duration_ms}'
```

A real porcelain reads `stdin` line by line, parses each as JSON,
matches on `t`, and renders accordingly. ~50 LOC in most languages.
