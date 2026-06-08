---
title: Tests with giant test
description: Test targets, selection, and CI patterns.
---

`giant test` is `giant build` restricted to `test: true` targets - same
matcher, same renderer. The one difference from a build target is the cache
default: **test targets run uncached by default**, so a plain test re-runs
every time and needs no outputs. Opt into caching and a test that already
passed on its current inputs is skipped until something changes.

## Marking a target as a test

The minimal form: `test: true` and a command. No outputs, no caching - it
runs whenever you ask.

```yaml
# internal/auth/giant.yaml
- name: "test"
  inputs:
    - "**/*.go"
  test: true
  tags: ["lang=go", "kind=test"]
  cwd: "//"
  command: "go test ./internal/auth"
```

The target's label is `//internal/auth:test`. `test: true` is the only thing
separating it from a regular target: `giant build` skips it, `giant test`
selects it, and it defaults to `cache: false` (so the "cacheable target needs
outputs" rule doesn't apply - an uncached test needs none).

## Caching test results

To skip a test that can't have changed, opt it into the cache with
`cache: true` and give it a **marker output** - a file the command touches
only on success:

```yaml
- name: "test"
  inputs:
    - "**/*.go"
  test: true
  cache: true                       # opt in (tests are uncached by default)
  outputs:
    - "//test-cache/auth.ok"
  tags: ["lang=go", "kind=test"]
  cwd: "//"
  command: |
    go test ./internal/auth && touch test-cache/auth.ok
```

The marker (`test-cache/auth.ok`) is what gets cached - its existence is the
recorded "the test passed for these inputs." (A cacheable target needs an
output or an `exists:` check; the marker is the simplest output.) On unchanged
inputs the marker is restored and `go test` is skipped.

## Selection

```bash
giant test                          # all test targets
giant test //internal/auth:test     # one specific test
giant test //internal/...           # every test under internal/
giant test --tag fast               # only tests tagged fast
giant test --no-tag db              # all tests except DB-dependent
giant test --affected --base main   # tests touched by changes since main
```

`giant build` excludes test targets by default - running `giant build`
won't accidentally execute your test suite.

## Cache semantics

A cached test is correct only if its `inputs:` cover everything the test
reads. If `auth_test.go` reads a fixture under `testdata/auth/`, list it
(paths are package-relative):

```yaml
inputs:
  - "**/*.go"
  - "testdata/**/*"
```

Otherwise an edit to the fixture won't invalidate the cache and you'll get a
stale pass. An uncached test (the default) has no such risk - it always runs -
which is the safer choice for tests whose inputs are hard to pin down (ones
that hit the network, the clock, or shared state).

## CI pattern: only affected tests

```bash
# In CI:
giant test --affected --base "$CI_MAIN_BRANCH" --quiet
```

- `--affected --base main` selects only test targets whose inputs (or
  transitive deps) changed since main.
- `--quiet` reduces output to failures plus the summary.

Set up your CI to fail when the exit code is non-zero. Giant exits
non-zero when any test target failed.

## Test output

By default each test target's stdout/stderr is prefixed with the
target label and streamed live:

```console
$ giant test
[//internal/auth:test] === RUN   TestPassword
[//internal/auth:test] --- PASS: TestPassword (0.01s)
[//internal/auth:test] PASS
✓ BUILD   //internal/auth:test   124ms

[//internal/store:test] === RUN   TestCRUD
[//internal/store:test] --- FAIL: TestCRUD/Create (0.02s)
[//internal/store:test]     store_test.go:42: expected ID, got empty string
[//internal/store:test] FAIL
✗ FAIL    //internal/store:test  78ms  exit code 1

  FAIL  1 built · 0 cached · 1 failed  in 220ms
  failed: //internal/store:test
```

The renderer is the same one `giant build` uses - see [CLI
reference](/reference/cli/) for output controls.

## Failing tests don't fight in parallel

Test targets run in parallel by default. A failure in one doesn't stop
others - the build runs to completion so you see every failure, not
just the first. The exit code is non-zero if any test failed.

There's no hard fail-fast flag. `-j1` runs targets one at a time, and a
failure stops anything *downstream* of it - but independent test targets
still run, so you don't get true stop-on-first-failure:

```bash
giant test -j1
```

Re-run just what broke last time with `failed-last`:

```bash
giant test failed-last
```

## Test-only deps

Sometimes a test needs a setup target that production doesn't (e.g. a
test database container). Express it as a regular dep:

```yaml
# internal/store/giant.yaml
- name: "fixtures-db"
  command: "tools/start-test-db.sh"
  cwd: "//"
  cache: false
  test: true

- name: "test"
  inputs: ["**/*.go"]
  outputs: ["//test-cache/store.ok"]
  deps: ["//internal/store:fixtures-db"]
  test: true
  cwd: "//"
  command: "go test ./internal/store && touch test-cache/store.ok"
```

`giant test` runs both - `fixtures-db` is pulled in as a dep of `test`.
`giant build` runs neither: both carry `test: true`, so the default build
excludes them. (Drop `test: true` from `fixtures-db` and a plain
`giant build` would start your test database - keep it on.)
