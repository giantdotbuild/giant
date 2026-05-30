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

### Build lifecycle

```jsonc
{ "t": "build.started",
  "id": "b_4f9c",
  "selection": ["go:bin:*"],
  "target_ids": ["go:bin:server", "go:bin:client"],
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
  "id": "go:bin:server",
  "deps": ["proto:gen"] }

{ "t": "target.started",
  "build": "b_4f9c",
  "id": "go:bin:server",
  "cache_key": "3a7f9c...",
  "command": "go build -o bin/server ./cmd/server" }

{ "t": "target.log",
  "build": "b_4f9c",
  "id": "go:bin:server",
  "stream": "stdout",       // or "stderr"
  "line": "go: downloading github.com/...",
  "truncated": false }      // present and true if the line was cut short

{ "t": "target.finished",
  "build": "b_4f9c",
  "id": "go:bin:server",
  "result": "built",        // built | cache_hit | remote_cache_hit | external_cache_hit | skipped | failed
  "duration_ms": 1240,
  "exit_code": 0,
  "outputs": ["bin/server"],
  "error": null }
```

### Watch

```jsonc
{ "t": "watch.started", "filter": "go:**" }
{ "t": "watch.batch",
  "paths": ["src/main.go"],
  "more": 0,
  "config_changed": false }
{ "t": "watch.affected", "target_ids": ["go:bin:server"] }
{ "t": "watch.state", "state": "building" }    // idle | building | building_with_pending | reloading_config | config_error
{ "t": "watch.stopped" }
```

### Discovery

```jsonc
{ "t": "discovery.merged",
  "build": "b_bootstrap_4f9c",
  "id": "discover:go",
  "added_targets": ["go:pkg:internal/auth", "go:pkg:internal/store"] }
```

### Affected subscription

Fired in response to `affected.subscribe`. The first event is the
initial snapshot; the engine then keeps a file watcher pinned and
re-emits whenever the affected set changes.

```jsonc
{ "t": "affected.changed",
  "base": "main",
  "target_ids": ["go:bin:server", "rust:lib:core"] }

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
  "target": "go:bin:server" }
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
  "targets": ["go:bin:server"],
  "fresh": false }

{ "c": "watch.start",
  "command_id": "c_2",
  "targets": ["go:bin:server"] }

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

{ "c": "shutdown",
  "command_id": "c_7" }
```

Each command is acknowledged with a `command.accepted` (carrying the
allocated `build_id` if applicable) or `command.rejected` (with a
reason - e.g. `"watch is active - send watch.stop first"`).
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
