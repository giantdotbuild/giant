---
title: Cache layout
description: What lives in the cache directory and why.
---

The local cache is a directory tree Giant owns. The default location
is `~/.cache/giant`; you can override per-workspace via `cache.dir`.

## Directory tree

```
~/.cache/giant/
├── version              # plain text, the integer line `1\n`
├── ac/                  # action cache - cache key → outputs
│   └── 3a/
│       └── 3a7f9c4e8b2d1f5e6a8c9d7e4f3b2a1c5d6e9f8a7b4c3d2e1f5a6b7c8d9e.json
├── cas/                 # content-addressed store - output bytes
│   └── 9f/
│       └── 9f3c8d7e2a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b
├── log/                 # reserved; captured logs currently live in cas/
└── tmp/                 # transient write-then-rename staging
```

All hex-named files are sharded by their first two hex characters
(256 directories). Keeps any single directory's entry count bounded.

## Permissions

The cache root is created with mode `0o700`. Files written inside
are `0o600`. Single-user assumption.

## `version`

A single integer line - the bytes `1\n` (no JSON quoting).
The cache layout schema version. If a binary expects a different
version, it errors instead of silently using the wrong shape.

## `ac/`

One JSON file per (target, cache key) pair. The cache key in hex is
both the filename and the lookup index:

```jsonc
{
  "schema": 1,
  "target_id": "//src/go/server:server",
  "cache_key": "3a7f9c...",
  "command": "go build -o bin/server ./cmd/server",
  "cwd": "",
  "outputs": [
    {
      "path": "bin/server",
      "content_hash": "9f3c8d...",
      "size": 8192000,
      "executable": true,
      "mode": "0755"
    }
  ],
  "outputs_content_hash": "abcd...",
  "exit_code": 0,
  "duration_ms": 1240,
  "built_at": "2026-05-20T13:24:01Z",
  "stdout_blob": "7c1e9a...",
  "stderr_blob": null
}
```

`outputs_content_hash` is the hash-of-hashes across `outputs[].content_hash`.
Downstream targets reference it (not the cache key) for the [early
cutoff](/concepts/cache-key/#early-cutoff) optimization.

`stdout_blob` / `stderr_blob` are the CAS hashes of the captured
stdout and stderr from the build that wrote this entry. Either can be
`null` - if the stream produced no bytes, or if `cache.capture_logs`
was off at the time. See [Log capture and replay](#log-capture-and-replay)
below.

## `cas/`

Pure content-addressed bytes. Filename = SHA-256 of contents (verified
on read). No metadata, no JSON - just the raw output bytes from some
target. Captured stdout/stderr blobs live in `cas/` too; they're CAS
entries like any other.

## `tmp/`

Write-then-rename staging. Filenames include the PID and a counter so
concurrent writers don't collide. Empty between operations (any
leftover means a previous `giant` invocation died mid-write - safe to
ignore or `giant clean`).

## Eviction

The cache evicts AC entries by LRU (file mtime) when the total size
exceeds `cache.max_size_gb * cache.evict_when_above_pct / 100`,
trimming back to `cache.evict_target_pct / 100`. CAS blobs are removed
along with their last referencing AC entry.

Entries with mtime within the last 5 minutes are skipped - a recency
buffer that protects in-flight builds running in another terminal.

Eviction runs after every successful build, in-process, silently. No
periodic timer, no `giant gc`.

See `cache.max_size_gb` in [giant.yaml reference](/reference/config/).

## Sharing across workspaces

Multiple workspaces can share one `cache.dir`. The cache key is purely
content-addressed - it covers the command, cwd, environment, inputs, and
dependency output hashes, but **not** the workspace name or target label.
So two workspaces that build a target with an identical recipe land on the
same key and share the entry; that's intended reuse (same recipe =
same output). CAS blobs deduplicate the same way (same bytes = same hash =
same path).

Eviction works fine across multi-workspace caches, but doesn't
fair-share. If one workspace dominates, it'll evict the other's older
entries. Document this for shared environments.

## Log capture and replay

Cache hits would otherwise be silent - you'd see `CACHE  //src/go/server:server`
and nothing else, even if the original build emitted useful diagnostic
output. To fix that, Giant captures each target's stdout and stderr
into CAS blobs alongside its outputs. On a hit, those blobs are read
back and re-emitted as `target.log` events, just like the live run.

### What gets stored

After a target builds successfully:

1. Streaming stdout/stderr is accumulated into in-memory buffers
   (capped - see below).
2. Each non-empty stream is written to `cas/` as a normal CAS blob.
3. The blob hashes go into the AC entry's `stdout_blob` /
   `stderr_blob` fields.

Failed builds don't write an AC entry at all, so their logs aren't
captured - failure output already streamed live, and there's nothing
to replay against.

### Replay

On a cache hit (local *or* remote), the executor reads the blobs from
local CAS and emits one `target.log` event per line. Renderer output
on a hit therefore matches a fresh build's output, modulo the
`CACHE`/`BUILD` verb. Porcelains that listen on `target.log` get the
replay automatically.

For a remote hit, the log blobs are fetched into local CAS alongside
the output blobs, so the next local hit replays without touching the
remote again.

### Caps

Each stream is capped at `cache.log_capture_cap_bytes` (default 5 MiB).
A build that exceeds the cap continues streaming live to the
console - only the captured portion stops growing. The on-disk blob
ends with `[giant: log truncated at capture cap]` so a future replay
makes the cutoff visible.

ANSI control sequences are preserved (raw bytes go in), so colors
replay too.

### Opting out

Both halves are independently configurable. Set
`cache.capture_logs: false` to skip writing log blobs entirely;
set `cache.replay_logs: false` to keep capturing but stay silent on
hits. See [config reference](/reference/config/) for the full shape.

### Eviction

Log blobs participate in normal eviction. When an AC entry is evicted,
its referenced `stdout_blob`/`stderr_blob` hashes are eligible for
collection. A subsequent cache hit on an entry whose log blobs were
already evicted just silently skips the replay - the hit itself still
succeeds.
