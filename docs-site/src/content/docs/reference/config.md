---
title: giant.yaml
description: Full configuration schema.
---

Giant reads `giant.yaml` (or `giant.json`) in your workspace root. The
full schema:

```yaml
workspace:
  name: <required>

cache:
  dir: ~/.cache/giant
  max_size_gb: 20
  evict_when_above_pct: 100
  evict_target_pct: 80
  capture_logs: true
  replay_logs: true
  log_capture_cap_bytes: 5242880

remote:                       # feature-gated; only with --features remote
  url: "https://cache.example.com"
  auth:
    kind: bearer
    token_env: GIANT_REMOTE_TOKEN
  tls:
    skip_verify: false

include:                      # discovery targets, run during bootstrap
  - id: "discover:go"
    inputs: [...]
    outputs: [...]
    command: "..."

targets:
  - id: "<unique>"
    inputs: [...]
    outputs: [...]
    deps: [...]
    command: "..."
    cwd: "..."
    env: { KEY: VAL }
    test: false
    tags: [release, linux]
    cache: true
    remote_cache: true
    exists: "..."
    timeout: 300
```

## `workspace`

| Field | Required | Description |
|---|---|---|
| `name` | yes | Workspace name. Alphanumeric, `-`, `_`. Used in cache keys. |

## `cache`

| Field | Default | Description |
|---|---|---|
| `dir` | `~/.cache/giant` | Local cache directory. Tildes expand. |
| `max_size_gb` | `20` | Max cache size in GB. `null` or `0` disables auto-eviction. |
| `evict_when_above_pct` | `100` | Trigger eviction at this percentage of max. |
| `evict_target_pct` | `80` | Evict down to this percentage when eviction runs. |
| `capture_logs` | `true` | Write each successful target's stdout + stderr to CAS so they can replay on a future cache hit. |
| `replay_logs` | `true` | On a cache hit (local or remote), re-emit the captured stdout/stderr as `target.log` events. |
| `log_capture_cap_bytes` | `5242880` (5 MiB) | Per-stream cap on captured bytes. Live streaming is unaffected; only the on-disk blob is truncated. |

The two-threshold setup avoids "always-evicting" behavior: trigger at
100%, evict down to 80%, leaving a 20% buffer before the next round.

Log capture/replay is what makes cache hits informative: without it
you'd see `CACHE go:bin:server` and nothing else, even if the original
build printed test failures, deprecation warnings, or compiler hints.
With it the renderer (and any porcelain on the [event protocol](/reference/events/))
sees the same `target.log` line stream a fresh build would have
produced. See [Log capture and replay](/reference/cache-layout/#log-capture-and-replay)
for storage details.

## `remote` (feature-gated)

| Field | Description |
|---|---|
| `url` | Cache endpoint (Bazel HTTP cache protocol). |
| `auth.kind` | `none`, `bearer`, or `basic`. |
| `auth.token_env` | (bearer) env var name to read the token from. |
| `auth.username_env` | (basic) env var name for the username. |
| `auth.password_env` | (basic) env var name for the password. |
| `tls.skip_verify` | If true, skip TLS cert verification. Don't use in production. |

## `include` and `targets`

Both lists hold target definitions with identical schema. The
difference: `include:` targets run during the bootstrap pass (before
the main build), and their outputs are JSON files Giant merges into
the graph. See [Discovery](/concepts/discovery/).

### Target fields

| Field | Required | Type | Description |
|---|---|---|---|
| `id` | yes | string | Unique target ID. Convention: `lang:kind:name`. |
| `inputs` | no | list | File globs and/or structural inputs. |
| `outputs` | no | list | Files the command produces, relative to `cwd`. |
| `deps` | no | list of strings | Additional explicit dependencies. |
| `command` | yes* | string | Shell command. Required unless `exists` is set. |
| `cwd` | no | string | Working dir, workspace-relative. Default: workspace root. |
| `env` | no | map | Env vars. Hashed into the cache key. |
| `test` | no | bool | `true` = test target. Default `false`. |
| `tags` | no | list of strings | Free-form labels for filtering. |
| `cache` | no | bool | `false` disables caching entirely. Default `true`. |
| `remote_cache` | no | bool | `false` disables remote uploads for this target. Default `true`. |
| `exists` | no | string | Shell command. Exit 0 → skip the build command. |
| `timeout` | no | int | Seconds before the command is killed. Default: no timeout. |

### Input shapes

**File glob (string form):**

```yaml
inputs:
  - "src/**/*.go"
  - "go.mod"
```

**Structural input:**

```yaml
inputs:
  - kind: structural
    files: "**/*.go"
    lines: ["package ", "import ", "//go:embed "]
```

See [Structural inputs](/concepts/structural-inputs/) for the full
story. The `files:` can be a string or list.

## Schema version

```yaml
schema_version: 1
```

Optional; defaults to `1`. Bumping to a future major version unlocks
new fields and may break older Giant binaries.

## Unknown-field handling

Most top-level structs are `deny_unknown_fields`. A typo in a field
name fails the config load - better than silently ignoring it.

```console
$ giant build
error: unknown field `inptus`, expected one of `id`, `inputs`, ...
```
