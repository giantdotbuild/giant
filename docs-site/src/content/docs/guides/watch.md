---
title: Watch mode
description: Continuous rebuilds with a debouncer that respects your selection.
---

`giant watch` runs an initial build, then keeps rebuilding affected
targets as files change. It's the everyday development loop.

## Basic usage

```bash
giant watch
```

This:

1. Runs an initial build of all non-test targets.
2. Starts watching the workspace for file changes.
3. On each batch of changes, computes which targets are affected and
   rebuilds them.
4. Loops until Ctrl-C.

## Selecting what to watch

The same selection language `giant build` uses applies:

```bash
giant watch go:bin:server         # one binary
giant watch 'go:**' '!go:test:*'  # everything Go, no tests
giant watch --tag dev             # dev-tagged targets
```

Watch enforces the selection on every cycle. Editing a file that only
affects targets *outside* the selection produces:

```
· no targets affected
```

- no rebuild, even though a file changed.

## The debouncer

Editor saves usually arrive in bursts (your editor writes the file,
its `.swp`, the lockfile, etc.). The watcher coalesces these into
batches:

- **Quiet window** (default 100 ms): a batch flushes after this long
  with no new events.
- **Max delay** (default 500 ms): a batch flushes after this long even
  if events keep streaming.

Both are tunable:

```bash
giant watch --quiet-ms 200 --max-delay-ms 1000
```

## What's excluded from the watch

By default Giant ignores:

- `.git/`
- `.giant/` (its own runtime state)
- The cache directory (whatever `cache.dir` resolves to)
- Every target's declared `outputs:` - Giant won't trigger rebuilds on
  files it produces itself

You don't need to write a `.giantignore`. Things you don't declare as
inputs don't trigger rebuilds anyway (a file change without an input
match means no target is affected → cycle skipped).

## Test loop

## Test-driven feedback loop

```bash
giant watch --test
```

This is the TDD shape: edit a source file, watch re-runs the affected
test targets, see results, repeat. Internally it's `giant watch` with
`TestMode::Only` - same selection semantics as `giant test`, same
debouncer, same affected-detection.

Mix with patterns and tags to scope further:

```bash
giant watch --test go:test:auth         # only auth tests
giant watch --test --tag fast           # only fast-tagged tests
giant watch --test go:test:* '!go:test:integration:*'
```

If you want everything - tests AND production targets - together:

```bash
giant watch --all
```

`--test` and `--all` are mutually exclusive; without either, the
default is "non-test targets only," matching `giant build`'s rule.

## Discovery re-runs

Watch re-prepares the graph on every cycle. If a structural input
(e.g. `import` lines in a Go file) changes, discovery re-runs and the
graph picks up newly emitted targets. New tests added during a watch
session are seen on the next cycle.

## Performance

The watcher uses `notify` (which uses inotify/FSEvents/ReadDirectoryChangesW
under the hood - kernel-level file notifications). Idle CPU usage is
near zero. The cycle work (re-prepare, affected computation, build)
dominates only when files actually change.

For a 10k-file workspace, the no-change side of a cycle is sub-50ms.

## Exit cleanly

Ctrl-C sends SIGINT. Giant cancels the in-flight build (if any), drains
the event queue, prints a "cancelled" note, and exits 0.

## Related: `giant task <name> --watch`

`giant watch` rebuilds *targets*. To re-run a *task* (lint, fmt,
deploy, an ad-hoc script) on file changes, use the task-runner's
own `--watch`:

```bash
giant task test:unit --watch
```

Declare `inputs:` on the task to narrow what triggers a re-run. See
[`giant-task`](/extending/giant-task/#watch-mode).
