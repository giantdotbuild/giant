---
title: Remote cache
description: Share build artifacts across machines via the Bazel HTTP cache protocol.
---

Giant's remote cache speaks the [Bazel HTTP cache
protocol](https://bazel.build/remote/caching) - the same protocol used
by `bazel-remote`, BuildBuddy, sccache, and various S3-backed servers.
That means you can:

- Run bazel-remote on a server you own
- Sign up for a hosted service
- Point Giant at sccache for shared Rust artifacts
- Use any S3 bucket via a tiny shim

…and Giant will use it without modification.

## Enable the feature

The remote cache lives behind a feature flag so the default binary
stays small and dependency-light:

```bash
cargo install --path crates/giant --features remote
```

Pre-built binaries from giant.build/install.sh include the remote
feature.

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
failure here doesn't fail the build (you get a warning).

## Bring up a bazel-remote server

```bash
docker run -d \
  -p 8080:8080 \
  -v /var/cache/bazel-remote:/data \
  buchgr/bazel-remote:latest \
  --dir /data --max_size 50
```

That's an open, unauthenticated cache. Point Giant at it:

```yaml
remote:
  enabled: true
  url: "http://localhost:8080"
```

For auth, see bazel-remote's docs - Giant supports the bearer-token
flow it offers.

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
