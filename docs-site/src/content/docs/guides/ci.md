---
title: CI integration
description: Patterns for using Giant in CI pipelines.
---

Giant in CI is the same Giant you run locally, with two pieces glued
on: a remote cache so builds across machines share work, and
`--affected` so PR builds only build what changed.

## The basic shape

```yaml
# .github/workflows/ci.yml
name: ci
on: [push, pull_request]
jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0           # we need history for --base
      - uses: yourorg/install-giant@v1
      - run: |
          giant build --affected --base origin/${{ github.base_ref || 'main' }} --quiet
          giant test --affected --base origin/${{ github.base_ref || 'main' }} --quiet
        env:
          GIANT_REMOTE_TOKEN: ${{ secrets.GIANT_REMOTE_TOKEN }}
```

Notes:

- `--affected --base origin/main` makes Giant build only what changed.
  On pushes to `main`, `github.base_ref` is empty - fall through to
  `main` so the diff is "since the merge base" (always empty for a
  fast-forward; the build will be a no-op cache-hit if you ran the same
  commit before).
- `--quiet` strips per-target lines; you see only failures plus the
  summary.

## Guard generated config

If the repo [generates config](/guides/generating-config/), the generated
`giant.<name>.yaml` files are committed, so build and test jobs run from
the checkout as-is - no generator installed, no generation step. Add one
job that catches drift between the generators and what's committed:

```yaml
  gen-check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: yourorg/install-giant@v1
      - run: giant gen --check
```

This job is the exception that needs `giant-gen` on PATH, plus whatever
toolchains the generators query (`go list`, `cargo metadata`). It exits
non-zero with a per-generator report when a `giant gen` run would change
the committed files.

## The zero-infrastructure remote cache

On GitHub Actions, Giant can use the runner's own cache service - no
server to host. Set `remote: { enabled: true, kind: github_actions }`
in `giant.yaml` and export the runner's credentials before the build
step; the [remote cache guide](/guides/remote-cache/#the-github-actions-cache)
has the two-line export and the scoping details. Outside Actions the
same config is inactive, so it's safe to commit.

## Bring your own remote cache

Set up a [bazel-remote](https://github.com/buchgr/bazel-remote) (or
sccache, or any Bazel-HTTP-cache-compatible server) and point Giant at
it:

```yaml
# giant.yaml (workspace root - remote lives in the root config only)
remote:
  enabled: true
  url: "https://cache.example.com"
  auth:
    kind: bearer
    token_env: GIANT_REMOTE_TOKEN
```

In CI, set `GIANT_REMOTE_TOKEN`. Locally, set it (or not - Giant works
offline without a remote cache).

## The "only build affected" pattern

```bash
giant affected --base origin/main
```

This prints the labels of targets that would rebuild, one per line,
without actually running anything. Useful for:

- Driving downstream jobs in matrix CI.
- Sanity-checking what a PR touches.
- Piping into `xargs giant build` for fine-grained control.

```bash
# Build only Go binaries that changed, no tests
giant affected --base origin/main --tag kind=bin --no-tag flaky | xargs -r giant build
```

## Sharded test runs

For large test suites, shard across runners:

```yaml
strategy:
  matrix:
    shard: [0, 1, 2, 3]
steps:
  - run: |
      tests=$(giant affected --base origin/main --tag kind=test | awk "NR%4==${{ matrix.shard }}")
      [ -z "$tests" ] && exit 0
      echo "$tests" | xargs giant test --quiet
```

A naive every-Nth split is fine for most repos. If your test
distribution is skewed, sort by historical duration.

## Caching the cache

GitHub Actions and most CI systems offer per-job filesystem cache.
Save Giant's local cache between runs to avoid re-downloading from the
remote on every job start:

```yaml
- uses: actions/cache@v4
  with:
    path: ~/.cache/giant
    key: giant-${{ runner.os }}-${{ hashFiles('giant.yaml', '**/Cargo.lock', '**/go.sum') }}
    restore-keys: |
      giant-${{ runner.os }}-
```

Even with a remote cache configured, this avoids the (small but
non-zero) cost of hitting the remote on a cold runner.

## Non-zero exit propagation

`giant build` and `giant test` exit non-zero when any target failed.
The renderer's summary block has already printed the failed targets,
so the CI logs read cleanly.

```console
$ giant build --quiet
✗ FAIL    //cmd/badthing:badthing  120ms  exit code 1

  FAIL  3 built · 12 cached · 1 failed  in 1.8s
  failed: //cmd/badthing:badthing
$ echo $?
1
```

## NDJSON for richer integrations

If your CI system has a structured-events ingest (BuildBuddy, your own
dashboard), use `--events ndjson`:

```bash
giant build --events ndjson > build.ndjson
```

Each line is one event; see the [event protocol
reference](/reference/events/) for the schema.
