---
title: giant.yaml
description: Full configuration schema.
---

Giant reads `giant.yaml` (or `giant.json`) from every package in the
tree - see [Config across the tree](#config-across-the-tree) below. The
root file holds the workspace-global sections; a nested package file
carries only `targets:`. The full schema (root file shown):

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

std:                          # giant-gen: where @std// generator modules come from
  ref: v3                     # giant-std tag or commit sha
  repo: giantdotbuild/giant-std
  # or instead of a pin:  path: ~/Development/giant-std

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

targets:
  - name: "<unique-in-package>"
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
    sandbox: true
    network: false
    exists: "..."
    timeout_secs: 300
```

## Config across the tree

Config is split across the tree. Every directory with a `giant.yaml` (or
`giant.json`) is a **package**; its file declares that package's targets.
The engine scans the whole workspace, reads every package file, and merges
them into one graph. See [Packages and labels](/concepts/packages/) for
the full model.

The **root** `giant.yaml` is mandatory. It marks the workspace (what `//`
resolves against) and is the only file that may carry the workspace-global
sections - `workspace`, `cache`, `remote` - plus the porcelain-owned
`tasks:` / `services:` / `std:` blocks. A **nested package file carries only
`targets:`**; putting a root-only section (`workspace`, `cache`, `remote`,
`tasks`, `services`) in a package file fails the config load loudly; it is
never silently ignored.

```yaml
# crates/giant/giant.yaml  →  package //crates/giant
targets:
  - name: "giant"          # identity: //crates/giant:giant
    inputs: ["src/**/*.rs", "Cargo.toml", "//Cargo.lock"]
    outputs: ["//bin/giant"]
    command: "cargo build --release -p giant"
```

A small project can keep every target in the root file (all `//:name`);
splitting earns its keep when a subdirectory is a natural unit of
ownership. For a large tree the package files are usually written by a
[generator](/guides/generating-config/) rather than by hand.

## Labels and identity

A target's engine identity is its **label**, derived from where it lives:
`//<package>:<name>`, where the package is the target's `giant.yaml`
directory (workspace-relative). A target named `server` in
`src/go/server/giant.yaml` is `//src/go/server:server`. A target in the
root file has the empty package, so it is `//:name`. The `name:` field
only has to be unique **within its own package** - two packages can each
have a `build` target without colliding. See
[Packages and labels](/concepts/packages/).

## Package-relative paths

Every path in a config file - `inputs`, `outputs`, `cwd`, and the
references that drive dependency inference - resolves relative to the
file's package:

- **Bare = package-relative.** `src/**/*.go` in `src/go/server/giant.yaml`
  means `src/go/server/src/**/*.go`.
- **`//` = workspace root.** `//Cargo.lock` is the root file regardless of
  which package references it; `//bin/giant` is a root-level output.
- **`cwd` defaults to the package directory.** Set `cwd: "//"` to run from
  the workspace root.

## `workspace`

Root file only.

| Field | Required | Description |
|---|---|---|
| `name` | yes | Workspace name. Alphanumeric, `-`, `_`. Marks this file as the workspace root (the scan walks up to the nearest config with a non-empty `name`) and is surfaced in events. Not part of the cache key. |

## `cache`

Root file only.

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
| `dir` | `.giant` | Per-workspace state directory. Holds build logs - anything Giant writes that's specific to this workspace (vs. content-addressed blobs, which live under `cache.dir`). Relative paths resolve under the workspace root. |

Splitting state from cache lets the cache live in a shared user-wide
directory while per-workspace state stays put. The default keeps
both backwards-compatible (state defaults to `.giant/` in the
workspace root, which is where it already was).

Log capture/replay is what makes cache hits informative: without it
you'd see `CACHE //src/go/server:server` and nothing else, even if the original
build printed test failures, deprecation warnings, or compiler hints.
With it the renderer (and any porcelain on the [event protocol](/reference/events/))
sees the same `target.log` line stream a fresh build would have
produced. See [Log capture and replay](/reference/cache-layout/#log-capture-and-replay)
for storage details.

## `std`

Root file only. Owned by the `giant-gen` porcelain: pins the
[`@std//` generator collection](/guides/generating-config/#pinning-the-std-collection).

| Field | Default | Description |
|---|---|---|
| `ref` | - | A giant-std tag or commit sha. Modules are fetched once per (repo, ref) and cached under the cache dir. |
| `repo` | `giantdotbuild/giant-std` | The collection's GitHub `owner/name`. Only with `ref`. |
| `path` | - | A local collection directory instead of a pin (a checkout, a devenv-managed path). Tildes expand; relative paths anchor at the workspace root. Mutually exclusive with `ref`. |

One of `ref` or `path` is required.

## `remote` (feature-gated)

Root file only.

| Field | Default | Description |
|---|---|---|
| `enabled` | `false` | Must be `true` to use the remote cache. Remote is a no-op when false. |
| `kind` | `bazel_http` | `bazel_http` (any Bazel-HTTP cache server) or `github_actions` (the Actions runner's own cache - no `url`/`auth`; see [the guide](/guides/remote-cache/#the-github-actions-cache)). |
| `url` | - | Cache endpoint (Bazel HTTP cache protocol). `bazel_http` only. |
| `auth.kind` | - | `none`, `bearer`, or `basic`. |
| `auth.token_env` | - | (bearer) env var name to read the token from. |
| `auth.username_env` | - | (basic) env var name for the username. |
| `auth.password_env` | - | (basic) env var name for the password. |
| `tls.skip_verify` | `false` | If true, skip TLS cert verification. Don't use in production. |
| `skip_head` | `false` | Skip the HEAD existence check before upload. |
| `max_blob_size_mb` | `500` | Blobs larger than this (in MB) are not uploaded. |

## `targets`

Regular build targets. Schema below.

### Target fields

| Field | Required | Type | Description |
|---|---|---|---|
| `name` | yes | string | Target name, unique within its package. The engine identity is the path-derived label `//<package>:<name>` (root targets are `//:name`). See [Labels and identity](#labels-and-identity). |
| `inputs` | no | list | File globs whose matched files feed the cache key. Package-relative; `//` anchors the workspace root. |
| `outputs` | no | list | Files the command produces, package-relative (`//` for root-level). Each entry is a glob; a literal must exist, a glob captures all matches (≥1), named + glob compose. |
| `deps` | no | list of strings | Additional explicit dependencies, given as labels (`//pkg:name`). |
| `command` | yes* | string | Shell command. Required unless `exists` is set. |
| `cwd` | no | string | Working dir. Package-relative; `//` is the workspace root. Default: the package directory. |
| `env` | no | map | Env vars. Hashed into the cache key. |
| `test` | no | bool | `true` = test target. Default `false`. |
| `tags` | no | list of strings | Free-form labels for filtering. |
| `cache` | no | bool | `false` disables caching entirely. Default: `true` for normal targets, `false` for `test: true` targets (the engine computes `cache.unwrap_or(!test)`). |
| `remote_cache` | no | bool | `false` disables remote uploads for this target. Default `true`. |
| `sandbox` | no | bool | `false` exempts the target from sandboxing when a run opts in with `--sandbox` (or under `giant verify`). Default `true`. Has no effect on plain runs and is never a cache-key input. |
| `network` | no | bool | `true` grants the target network access when sandboxed. Default `false` (denied). Inert outside `--sandbox` mode; never a cache-key input. |
| `exists` | no | string | Shell command. Exit 0 → skip the build command. |
| `timeout_secs` | no | int | Seconds before the command is killed. Default: no timeout. |

### Input shapes

A bare string is a file glob; the explicit object form is equivalent:

```yaml
inputs:
  - "src/**/*.go"                  # string form
  - { kind: file, glob: "go.mod" } # object form
```

## Schema version

```yaml
schema_version: 1
```

Optional; defaults to `1`. Bumping to a future major version unlocks
new fields and may break older Giant binaries.

## Unknown-field handling

The structured config blocks - `workspace`, `cache`, `remote`, `state` -
are `deny_unknown_fields`, so a typo there fails the config load rather
than silently doing nothing:

```console
$ giant build
error: unknown field `maxsize`, expected one of `dir`, `max_size_gb`, ...
```

The top-level document and individual target entries are deliberately
**open**: porcelains (giant-task, giant-tui) add their own top-level
sections, and the engine ignores keys it doesn't recognise. A typo in a
target field name (`inptus:`) is therefore dropped silently rather than
erroring - a generator is the usual author of targets, so the schema
stays permissive on that side.
