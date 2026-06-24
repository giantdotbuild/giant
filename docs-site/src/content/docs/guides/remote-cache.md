---
title: Remote cache
description: Share build artifacts across machines - a Bazel HTTP cache server, or the GitHub Actions cache.
---

Giant's remote cache has two backends:

- The [Bazel HTTP cache protocol](https://bazel.build/remote/caching) -
  the same protocol used by `bazel-remote`, BuildBuddy, sccache, and
  various S3-backed servers. Run one anywhere, point Giant at it.
- The [GitHub Actions cache](#the-github-actions-cache): when CI runs
  on Actions, Giant uses the runner's own cache service directly -
  shared caching with zero infrastructure to host.

## Enable it

The remote client is compiled into every release binary, so there's
nothing to turn on at build time - set `remote.enabled: true` in
`giant.yaml` (below) and you're done. A source build picks it up by
default too:

```bash
cargo install --path crates/giant
```

If you build with `--no-default-features`, add `--features remote` to
put the client back.

## Configure

Remote cache settings live in the root `giant.yaml` only - a
workspace-global setting that a nested [package](/concepts/packages/) can't
carry.

```yaml
# giant.yaml (workspace root)
remote:
  enabled: true
  url: "https://cache.example.com"
  auth:
    kind: bearer
    token_env: GIANT_REMOTE_TOKEN
```

Auth shapes:

```yaml
# No auth (open cache, e.g. a private network bazel-remote)
remote:
  enabled: true
  url: "http://cache.internal:8080"

# Bearer token from env var
remote:
  enabled: true
  url: "https://cache.example.com"
  auth:
    kind: bearer
    token_env: GIANT_REMOTE_TOKEN

# HTTP Basic auth
remote:
  enabled: true
  url: "https://cache.example.com"
  auth:
    kind: basic
    username_env: GIANT_REMOTE_USER
    password_env: GIANT_REMOTE_PASS
```

## The GitHub Actions cache

Inside a GitHub Actions job, the runner already hosts a cache service -
the one `actions/cache` uses. Giant can speak to it directly:

```yaml
# giant.yaml (workspace root)
remote:
  enabled: true
  kind: github_actions
```

No `url`, no `auth` - the runner provides both. The catch is that the
runner only exposes its credentials (`ACTIONS_RESULTS_URL`,
`ACTIONS_RUNTIME_TOKEN`) to JavaScript actions, never to plain `run:`
steps, so the workflow exports them once before any `giant` step (the
same dance sccache and BuildKit require):

```yaml
- name: Expose the cache credentials to giant
  uses: actions/github-script@v7
  with:
    script: |
      core.exportVariable('ACTIONS_RESULTS_URL', process.env.ACTIONS_RESULTS_URL || '');
      core.exportVariable('ACTIONS_RUNTIME_TOKEN', process.env.ACTIONS_RUNTIME_TOKEN || '');

- run: giant build --quiet
```

The config commits once and behaves sensibly everywhere: outside
Actions, a `github_actions` remote is simply inactive and builds run
with the local cache only. Inside Actions with the export step missing,
giant fails at startup with an error naming the variables - the one
case that's a workflow bug rather than a normal environment.

Three GitHub-isms to know:

- **Branch scoping.** Entries written on the default branch are
  readable from every branch; a PR's writes are visible only to that
  PR. Net effect: `main` warms the cache, PRs read it, PRs can't
  pollute each other.
- **Quota.** GitHub gives each repo 10 GB, evicting least-recently-used
  entries. `cache.max_size_gb` doesn't apply here; GitHub does its own
  housekeeping.
- **Rate limits.** Heavily parallel builds can get throttled. Giant's
  remote is best-effort everywhere, so a throttled call degrades to a
  cache miss and the build carries on.

## How the lookup chain works

When Giant computes a cache key, it tries sources in this order:

1. **Local AC lookup.** Fastest - usually sub-millisecond.
2. **Remote AC lookup.** Network round-trip; succeeds if any machine
   built this key before. The remote AC entry tells us which CAS blobs
   to pull.
3. **Remote CAS download.** Pull each blob; verify hash; write to
   local AC + CAS for future fast-path.
4. **`exists` check** (if declared on the target). External resource
   already there? Skip the build.
5. **Local execution.** Run the command. After success, write local
   AC + CAS, queue upload to remote.

The upload queue runs in the background - the build doesn't wait for
it before moving to the next target. Uploads are best-effort; a
failure here never fails the build. When a write does fail, Giant logs
one error line (it won't repeat per target) so an enabled-but-inert
remote doesn't pass unnoticed.

## When the remote can't be set up

A remote that's `enabled` but can't start - missing credentials, an
unreadable config - doesn't fail the build either. Giant logs one error
and runs with the local cache only. So `remote.enabled: true` can sit in
a shared `giant.yaml` while a developer without the cache credentials
still builds locally. The one exception is a `github_actions` remote
running inside Actions with the credential-export step missing: that's a
workflow bug, so it fails at startup.

## Bring up a bazel-remote server

```bash
docker run -d \
  -p 8080:8080 \
  -v /var/cache/bazel-remote:/data \
  buchgr/bazel-remote:latest \
  --dir /data --max_size 50 \
  --disable_http_ac_validation
```

That's an open, unauthenticated cache. Point Giant at it:

```yaml
remote:
  enabled: true
  url: "http://localhost:8080"
```

For auth, see bazel-remote's docs - Giant supports the bearer-token
flow it offers.

### `--disable_http_ac_validation` is required

bazel-remote defaults to validating every action-cache write as a
[REAPI](https://bazel.build/remote/rpc) `ActionResult` protobuf. Giant
stores its own JSON in the AC, so without the flag bazel-remote rejects
every AC write with `400 Bad Request` - CAS blobs upload fine, but no
entry is ever readable, and the cache looks enabled yet never hits.
Giant logs one error when it sees the rejection. Run bazel-remote with
`--disable_http_ac_validation` and the AC accepts giant's entries.

## Permissions in shared caches

If two developers' workspaces produce the same cache key, they'll
share the cached outputs. Giant doesn't ship any sandboxing, so make
sure your build commands are reproducible:

- Pin toolchain versions so every machine keys the same - see
  [Pinning toolchains](/guides/toolchains/).
- Avoid embedding absolute paths in outputs.
- Don't depend on the user's `$HOME` or `$USER` unless they're listed
  in `env:`.

Test by running `giant build` on a different machine with an empty
local cache. If the remote cache hits and the outputs work, you're
good.

## Disabling remote uploads per target

```yaml
# cmd/server/giant.yaml  →  //cmd/server:server
- name: "server"
  remote_cache: false      # local cache only
```

Useful when:

- Outputs are larger than is worth shipping over the wire.
- Outputs contain non-portable data (machine-specific paths).
- The build is so fast that the network round-trip would be slower.

`cache: false` opts out of caching entirely - local and remote both.

## Inspecting cache behaviour

```console
$ giant build //cmd/server:server
↓ REMOTE  //cmd/server:server  120ms       # downloaded from remote
$ giant build //cmd/server:server
✓ CACHE   //cmd/server:server    2ms       # local cache hit
```

The verb tells you which layer answered.

The stdout/stderr of the cached invocation is replayed automatically on
any cache hit (local or remote) - see
[Log capture and replay](/reference/cache-layout/#log-capture-and-replay).
