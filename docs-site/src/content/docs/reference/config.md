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
  enabled: true               # must be true; remote is a no-op otherwise
  url: "https://cache.example.com"
  auth:
    kind: bearer
    token_env: GIANT_REMOTE_TOKEN
  tls:
    skip_verify: false
  skip_head: false
  max_blob_size_mb: 500

discovery:
  strict: false               # true = no `reads` manifest is an error

include:                      # discovery targets, run during bootstrap
  - id: "discover:go"
    command: "tools/discover-go.sh > .giant/d/go.json"
    outputs: [".giant/d/go.json"]
    scope: ["src/"]           # optional; bounds reads + narrows fsmonitor

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
    timeout_secs: 300
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
| `max_age_days` | - | Optional. Evict cache entries older than N days. |
| `evict_when_above_pct` | `100` | Trigger eviction at this percentage of max. |
| `evict_target_pct` | `80` | Evict down to this percentage when eviction runs. |
| `capture_logs` | `true` | Write each successful target's stdout + stderr to CAS so they can replay on a future cache hit. |
| `replay_logs` | `true` | On a cache hit (local or remote), re-emit the captured stdout/stderr as `target.log` events. |
| `log_capture_cap_bytes` | `5242880` (5 MiB) | Per-stream cap on captured bytes. Live streaming is unaffected; only the on-disk blob is truncated. |

The two-threshold setup avoids "always-evicting" behavior: trigger at
100%, evict down to 80%, leaving a 20% buffer before the next round.

## `state`

| Field | Default | Description |
|---|---|---|
| `dir` | `.giant` | Per-workspace state directory. Holds discovery sidecars, the fsmonitor token, build logs - anything Giant writes that's specific to this workspace (vs. content-addressed blobs, which live under `cache.dir`). Relative paths resolve under the workspace root. |

Splitting state from cache lets the cache live in a shared user-wide
directory while per-workspace state stays put. The default keeps
both backwards-compatible (state defaults to `.giant/` in the
workspace root, which is where it already was).

Log capture/replay is what makes cache hits informative: without it
you'd see `CACHE go:bin:server` and nothing else, even if the original
build printed test failures, deprecation warnings, or compiler hints.
With it the renderer (and any porcelain on the [event protocol](/reference/events/))
sees the same `target.log` line stream a fresh build would have
produced. See [Log capture and replay](/reference/cache-layout/#log-capture-and-replay)
for storage details.

## `remote` (feature-gated)

| Field | Default | Description |
|---|---|---|
| `enabled` | `false` | Must be `true` to use the remote cache. Remote is a no-op when false. |
| `url` | - | Cache endpoint (Bazel HTTP cache protocol). |
| `auth.kind` | - | `none`, `bearer`, or `basic`. |
| `auth.token_env` | - | (bearer) env var name to read the token from. |
| `auth.username_env` | - | (basic) env var name for the username. |
| `auth.password_env` | - | (basic) env var name for the password. |
| `tls.skip_verify` | `false` | If true, skip TLS cert verification. Don't use in production. |
| `skip_head` | `false` | Skip the HEAD existence check before upload. |
| `max_blob_size_mb` | `500` | Blobs larger than this (in MB) are not uploaded. |

## `discovery`

Workspace-level settings for discovery (`include:` entries).

| Field | Default | Description |
|---|---|---|
| `strict` | `false` | When `true`, a discovery whose output omits a `reads` manifest is a hard error instead of a warning. Useful in CI to enforce the cooperative protocol. |

## `dispatch`

How `giant <name>` routes when `<name>` is neither a built-in subcommand
nor a `giant-<name>` binary on PATH. Core reads this table and execs the
binary it names - it never learns what a task is. The default routes
everything to `giant-task`, so `giant <task>` works out of the box.

```yaml
# Shorthand: a single catch-all binary (the default).
dispatch:
  unknown: "giant-task"
```

```yaml
# Full form: ordered match → binary rules, first match wins.
dispatch:
  unknown:
    - { match: "db:*", to: "giant-dbtool" }   # giant db:migrate → giant-dbtool db:migrate …
    - { match: "*",    to: "giant-task" }      # everything else → giant-task
```

`match` is a glob over the subcommand name; the routed binary is exec'd
as `<to> <name> <args…>` with everything passed through. An explicit
`giant-<name>` binary on PATH still wins over the table. With no
`dispatch:` (or a missing/invalid config), routing falls back to the
default `* → giant-task`. See ADR-0021.

## `targets`

Regular build targets. Schema below.

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
| `cache` | no | bool | `false` disables caching entirely. Default: `true` for normal targets, `false` for `test: true` targets (the engine computes `cache.unwrap_or(!test)`). |
| `remote_cache` | no | bool | `false` disables remote uploads for this target. Default `true`. |
| `exists` | no | string | Shell command. Exit 0 → skip the build command. |
| `timeout_secs` | no | int | Seconds before the command is killed. Default: no timeout. |

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

## `include`

Discovery entries: subprocesses that emit JSON to be merged into the
graph. See [Discovery](/concepts/discovery/) for the workflow. The
field set differs from `targets:` in a few ways:

| Field | Required | Type | Description |
|---|---|---|---|
| `id` | yes | string | Unique target ID. |
| `command` | yes | string | The discovery command. |
| `outputs` | yes | list | The JSON file(s) the command writes. |
| `deps` | no | list of strings | Explicit dependencies (e.g. on a compiled discovery tool). |
| `inputs` | no | list | Explicit files to content-hash into the discovery cache key. Optional - Giant already pulls in any argv token that resolves to a workspace file (the script, an in-tree binary on `$PATH`, etc.). Use this to declare helpers and embedded data the argv walk can't see (sourced shell libraries, config templates). Same `File`/`Structural` shapes as `targets.inputs`. |
| `cwd` | no | string | Working dir, workspace-relative. |
| `env` | no | map | Env vars. Hashed into the discovery cache key. |
| `scope` | no | list of strings | Directory prefixes the discovery may read from. Used as the sandbox fence (if sandboxing is on) and the fsmonitor narrowing hint. Contributes to the cache key. |
| `exists` | no | string | Same as on regular targets. |
| `timeout_secs` | no | int | Same as on regular targets. |

The discovery cache key is
`cmd + cwd + env + scope + content`, where `content` is the deduped
union of the argv-walk hits and any `inputs:` matches. Cooperatively-
emitted `reads` from the script's JSON still drives the warm-path
verifier - the two mechanisms compose.

### Discovery output shape

The JSON each discovery's `command` writes:

```jsonc
{
  "schema_version": 1,
  "targets": [ /* TargetSpec entries, same schema as `targets:` above */ ],
  "include": [ /* nested discovery entries, processed recursively */ ],
  "reads": {
    "files": [
      { "path": "go.mod" },
      { "path": "main.go", "lines": ["package ", "import "] }
    ],
    "dirs": [
      { "path": "pkg/", "filter": "*.go" }
    ]
  }
}
```

`reads.files` entries take two forms:

- **Whole-file**: `{ "path": "go.mod" }` - the verifier hashes the
  entire file's contents. Any change invalidates.
- **Excerpt**: `{ "path": "...", "lines": ["^pkg ", "^import "] }`
  - the verifier hashes only the lines whose prefix matches any
  pattern. Function-body edits that don't touch those lines leave
  the recorded hash unchanged.

`reads.dirs` entries hash a directory's listing (no recursion). With
`filter:` set, only entry names matching any of the glob patterns
contribute. Single-string and array forms both work for `lines:` and
`filter:`.

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
