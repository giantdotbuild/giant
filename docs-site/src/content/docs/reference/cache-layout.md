---
title: Cache layout
description: What lives in the cache directory and why.
---

The local cache is a directory tree Giant owns. The default location
is `~/.cache/giant`; you can override per-workspace via `cache.dir`.

## Directory tree

```
~/.cache/giant/
├── version              # plain text, contains "1"
├── ac/                  # action cache - cache key → outputs
│   └── 3a/
│       └── 3a7f9c4e8b2d1f5e6a8c9d7e4f3b2a1c5d6e9f8a7b4c3d2e1f5a6b7c8d9e.json
├── cas/                 # content-addressed store - output bytes
│   └── 9f/
│       └── 9f3c8d7e2a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e6f7a8b
├── structural/          # per-target sidecars for structural inputs
│   └── 8e/
│       └── 8e1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b1c2d3e4f5a6b7c8d9e.json
├── log/                 # captured stdout/stderr blobs (planned)
└── tmp/                 # transient write-then-rename staging
```

All hex-named files are sharded by their first two hex characters
(256 directories). Keeps any single directory's entry count bounded.

## Permissions

The cache root is created with mode `0o700`. Files written inside
are `0o600`. Single-user assumption.

## `version`

Just `"1"`. The cache layout schema version. If a binary expects a
different version, it errors instead of silently using the wrong
shape.

## `ac/`

One JSON file per (target, cache key) pair. The cache key in hex is
both the filename and the lookup index:

```jsonc
{
  "schema": 1,
  "target_id": "go:bin:server",
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
  "stdout_blob": null,
  "stderr_blob": null
}
```

`outputs_content_hash` is the hash-of-hashes across `outputs[].content_hash`.
Downstream targets reference it (not the cache key) for the [early
cutoff](/concepts/cache-key/#early-cutoff) optimization.

## `cas/`

Pure content-addressed bytes. Filename = SHA-256 of contents (verified
on read). No metadata, no JSON - just the raw output bytes from some
target.

## `structural/`

Sidecars for structural inputs. One per target that uses any structural
input. Holds per-file `(mtime, size, filtered_hash)` tuples so warm
validation can skip re-reading unchanged files.

```jsonc
{
  "schema": 1,
  "entries": [
    {
      "spec": { "files": ["**/*.go"], "lines": ["package ", "import "] },
      "files": {
        "internal/auth/auth.go": { "mtime_ns": 1715890000000000000, "size": 4096, "hash": "abcd..." },
        ...
      },
      "fingerprint": "5c8a3f..."
    }
  ]
}
```

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

Multiple workspaces can share one `cache.dir` - they're isolated by
the workspace name being part of every cache key. CAS blobs naturally
deduplicate across workspaces (same bytes = same hash = same path).

Eviction works fine across multi-workspace caches, but doesn't
fair-share. If one workspace dominates, it'll evict the other's older
entries. Document this for shared environments.
