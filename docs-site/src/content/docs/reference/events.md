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
  "target_count": 42,
  "task_count": 0 }

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
  "added_targets": ["go:pkg:internal/auth", "go:pkg:internal/store"],
  "added_tasks": [] }
```

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

## Command channel (planned)

When `giant serve` lands, clients will be able to send commands back
via the same wire format:

```jsonc
{ "c": "build",
  "targets": ["go:bin:server"],
  "fresh": false }

{ "c": "selection.list_tags" }

{ "c": "selection.resolve",
  "patterns": ["go:**"],
  "tags": ["release"],
  "no_tags": ["flaky"],
  "test_mode": "exclude" }

{ "c": "cancel", "build": "b_4f9c" }
{ "c": "shutdown" }
```

Responses come back as normal events on the stream.

## Consuming the stream

The simplest consumer is `jq`:

```bash
giant build --events ndjson \
  | jq -c 'select(.t == "target.finished") | {id, result, duration_ms}'
```

A real porcelain reads `stdin` line by line, parses each as JSON,
matches on `t`, and renders accordingly. ~50 LOC in most languages.
