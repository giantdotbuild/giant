---
title: Tests with giant test
description: Test targets, selection, and CI patterns.
---

`giant test` is `giant build` for `test: true` targets. Same matcher,
same renderer, same caching semantics. A test that's already passed
on its current inputs doesn't run again until something changes.

## Marking a target as a test

```yaml
# internal/auth/giant.yaml
- name: "test"
  inputs:
    - "**/*.go"
  outputs:
    - "//test-cache/auth.ok"
  test: true
  tags: ["lang=go", "kind=test"]
  cwd: "//"
  command: |
    go test ./internal/auth && touch test-cache/auth.ok
```

The target's label is `//internal/auth:test`. The `test: true` field is
the only thing that separates it from a regular target. The output file
(`test-cache/auth.ok`) is what gets cached - its existence means "the
test passed for these inputs."

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

A test target caches the same way a build target does. If the inputs
are unchanged, the cache restores `test-cache/auth.ok` (the marker
file) and `go test` is skipped.

This is correct if and only if your test inputs cover everything the
test reads. If `auth_test.go` reads a fixture file under
`testdata/auth/`, list it (paths are package-relative):

```yaml
inputs:
  - "**/*.go"
  - "testdata/**/*"
```

Otherwise an edit to the fixture won't invalidate the cache and you'll
get a stale pass.

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
