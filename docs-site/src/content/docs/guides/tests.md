---
title: Tests with giant test
description: Test targets, selection, and CI patterns.
---

`giant test` is `giant build` for `test: true` targets. Same matcher,
same renderer, same caching semantics. A test that's already passed
on its current inputs doesn't run again until something changes.

## Marking a target as a test

```yaml
- id: "go:test:auth"
  inputs:
    - "internal/auth/**/*.go"
  outputs:
    - "test-cache/auth.ok"
  test: true
  command: |
    go test ./internal/auth && touch test-cache/auth.ok
```

The `test: true` field is the only difference from a regular target.
The output file (`test-cache/auth.ok`) is what gets cached - its
existence means "the test passed for these inputs."

## Selection

```bash
giant test                       # all test targets
giant test go:test:auth          # one specific test
giant test 'go:test:*'           # all Go tests
giant test --tag fast            # only tests tagged fast
giant test --no-tag db           # all tests except DB-dependent
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
`testdata/auth/`, list it:

```yaml
inputs:
  - "internal/auth/**/*.go"
  - "internal/auth/testdata/**/*"
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
target ID and streamed live:

```console
$ giant test
[go:test:auth] === RUN   TestPassword
[go:test:auth] --- PASS: TestPassword (0.01s)
[go:test:auth] PASS
✓ BUILD   go:test:auth   124ms

[go:test:store] === RUN   TestCRUD
[go:test:store] --- FAIL: TestCRUD/Create (0.02s)
[go:test:store]     store_test.go:42: expected ID, got empty string
[go:test:store] FAIL
✗ FAIL    go:test:store  78ms  exit code 1

  FAIL  1 built · 0 cached · 1 failed  in 220ms
  failed: go:test:store
```

The renderer is the same one `giant build` uses - see [CLI
reference](/reference/cli/) for output controls.

## Failing tests don't fight in parallel

Test targets run in parallel by default. A failure in one doesn't stop
others - the build runs to completion so you see every failure, not
just the first. The exit code is non-zero if any test failed.

To stop on first failure, run with `-j1` (sequential) and rely on the
fact that Giant won't start a downstream target once an upstream
fails:

```bash
giant test -j1
```

## Test-only deps

Sometimes a test needs a setup target that production doesn't (e.g. a
test database container). Express it as a regular dep:

```yaml
- id: "test:fixtures:db"
  command: "tools/start-test-db.sh"
  cache: false

- id: "go:test:store"
  inputs: ["internal/store/**/*.go"]
  outputs: ["test-cache/store.ok"]
  deps: ["test:fixtures:db"]
  test: true
  command: "go test ./internal/store && touch test-cache/store.ok"
```

`giant test` runs both. `giant build` runs neither (both opt out via
`test: true` on one and being only-test-relevant on the other).
